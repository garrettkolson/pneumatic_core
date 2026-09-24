//! Broadcast fan-out for `NodeRegistry`: async `send_to_all` and the
//! blocking variant, with per-(rhash, type) delivery-failure counters.

use super::*;

impl NodeRegistry {
/// Send data to all registered nodes of a given type (async, concurrent).
/// Each send is bounded by `self.send_timeout` so a hung route or socket
/// can't pin a tokio worker thread (H7), and every failed delivery is
/// recorded + logged via `record_delivery_failure` (Phase 6.2) so a lost
/// send is observable. Both the RNS and the direct branches fan out
/// concurrently (`join_all`).
pub async fn send_to_all(&self, data: Vec<u8>, node_type: &NodeRegistryType) {
    let Some(nodes) = self.get_nodes(node_type) else { return };

    // If RNS transport is available, send via RNS. Fan out concurrently —
    // each peer's send is an independent `join_all` future.
    if let Some(network) = &self.network {
        // Collect rhashes to release DashMap guards
        let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();
        let mut rhashes = Vec::new();
        for key in keys {
            if let Some(entry) = nodes.get(&key) {
                rhashes.push(entry.value().rhash);
            }
        }
        let send_futs: Vec<_> = rhashes.into_iter().map(|rhash| {
            let node_type = node_type.clone();
            let failures = Arc::clone(&self.delivery_failures);
            let network = Arc::clone(network);
            let send_data = data.clone();
            async move {
                // Off the runtime thread (spawn_blocking) and bounded
                // (time::timeout): a blocked RNS send degrades to
                // Err(Timeout) instead of hanging the caller.
                match bounded_send_async(self.send_timeout, move || {
                    let _ = RnsSender::new(network, rhash).get_response(&send_data);
                })
                .await
                {
                    Ok(()) => {}
                    Err(e) => record_delivery_failure(&failures, rhash, &node_type, e),
                }
            }
        }).collect();
        join_all(send_futs).await;
        return;
    }

    // Collect keys to release DashMap guards, then send concurrently.
    let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();

    // Use get() for each connection individually (simpler, avoids lifetime issues).
    // The `async_trait` `send` future borrows `&entry`, so the DashMap guard
    // is held across the `.await` (same as before); copy `rhash` out first.
    // Each future owns fresh clones of the shared state so it can record
    // its own result without borrowing the enclosing scope.
    let send_futs: Vec<_> = keys.into_iter()
        .map(|key| {
            let node_type = node_type.clone();
            let failures = Arc::clone(&self.delivery_failures);
            let nodes_clone = Arc::clone(&nodes);
            let send_data = data.clone();
            async move {
                if let Some(entry) = nodes_clone.get(&key) {
                    let rhash = entry.value().rhash;
                    match tokio::time::timeout(self.send_timeout, entry.value().conn.send(&send_data)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => record_delivery_failure(&failures, rhash, &node_type, e),
                        Err(_) => record_delivery_failure(
                            &failures, rhash, &node_type,
                            ConnError::Timeout(format!("direct send exceeded {:?}", self.send_timeout)),
                        ),
                    }
                }
            }
        })
        .collect();
    join_all(send_futs).await;
}

/// Blocking version for sync contexts (runs sends sequentially). Each send
/// is bounded by `self.send_timeout` and runs on a detached std thread, with no
/// ambient runtime assumed (H7): a hung RNS route or socket degrades to
/// `Err(ConnError::Timeout)` instead of hanging the caller. Every failed
/// delivery is recorded + logged (Phase 6.2).
pub fn send_to_all_blocking(&self, data: Vec<u8>, node_type: &NodeRegistryType) {
    let Some(nodes) = self.get_nodes(node_type) else { return };

    // If RNS transport is available, send via RNS
    if let Some(network) = &self.network {
        // Collect rhashes to release DashMap guards
        let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();
        let mut rhashes = Vec::new();
        for key in keys {
            if let Some(entry) = nodes.get(&key) {
                rhashes.push(entry.value().rhash);
            }
        }
        let failures = Arc::clone(&self.delivery_failures);
        for rhash in rhashes {
            let network = Arc::clone(network);
            let send_data = data.clone();
            match bounded_send(self.send_timeout, move || {
                let _ = RnsSender::new(network, rhash).get_response(&send_data);
            }) {
                Ok(()) => {}
                Err(e) => record_delivery_failure(&failures, rhash, node_type, e),
            }
        }
        return;
    }

    // Direct-connection branch. This path must *actually* send, not drop a
    // detached future: the `async_trait` `send` future needs a runtime to
    // drive, and a detached std thread has none. Build a self-contained
    // `current_thread` runtime here (no ambient runtime assumed — consistent
    // with 6.1) and `block_on` each send. A failed send or elapsed bound is
    // recorded + logged (Phase 6.2).
    // Direct-connection branch. This path must *actually* send, not drop a
    // detached future: the `async_trait` `send` future needs a runtime to
    // drive, and a detached std thread has none. Build a self-contained
    // `current_thread` runtime here (no ambient runtime assumed — consistent
    // with 6.1) and `block_on` each send. A failed send or elapsed bound is
    // recorded + logged (Phase 6.2).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build local runtime for blocking direct send");

    // Collect (key, rhash) pairs up front to release DashMap guards before
    // driving each send on the local runtime.
    let keys: Vec<Vec<u8>> = nodes.iter().map(|e| e.key().clone()).collect();
    let peers: Vec<(Vec<u8>, [u8; 16])> = {
        let mut v = Vec::new();
        for key in keys {
            if let Some(entry) = nodes.get(&key) {
                v.push((key, entry.value().rhash));
            }
        }
        v
    };

    let failures = Arc::clone(&self.delivery_failures);
    for (key, rhash) in peers {
        let nodes_for_send = Arc::clone(&nodes);
        let send_data = data.clone();
        let result = runtime.block_on(async move {
            if let Some(entry) = nodes_for_send.get(&key) {
                tokio::time::timeout(self.send_timeout, entry.value().conn.send(&send_data)).await
            } else {
                Ok(Ok(()))
            }
        });
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => record_delivery_failure(&failures, rhash, node_type, e),
            Err(_) => record_delivery_failure(
                &failures, rhash, node_type,
                ConnError::Timeout(format!("direct send exceeded {:?}", self.send_timeout)),
            ),
        }
    }
}

/// Count of failed fan-out deliveries for a specific (rhash, node_type).
/// Test accessor for Phase 6.2 observability.
pub fn failure_count(&self, rhash: [u8; 16], node_type: &NodeRegistryType) -> u64 {
    self.delivery_failures
        .get(&(rhash, node_type.clone()))
        .map(|c| *c.value())
        .unwrap_or(0)
}

/// Total failed fan-out deliveries across every (rhash, node_type) key.
/// Test accessor for Phase 6.2 observability.
pub fn total_delivery_failures(&self) -> u64 {
    self.delivery_failures.iter().map(|e| *e.value()).sum()
}
}
