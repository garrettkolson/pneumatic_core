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
    // Same-host hop first (see `self_delivery`): on a composite this host is a
    // member of the target set and is not in its own bucket, so this copy is
    // owed and no peer send will carry it. Delivered before the peer fan-out so
    // a hung peer route cannot delay a hop that never leaves the machine.
    self.deliver_self_copy(&data, node_type);

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

/// Targeted fan-out to a named set of keys (async), with the same delivery
/// accounting and the same same-host rule as [`Self::send_to_peers_blocking`].
///
/// This is the async twin that ADR-004 needed and did not exist. The finalize
/// hop is single-recipient — the transaction names one Finalizer — and the role
/// that must make that hop (`Executor::send_to_finalizer`) is `async` and runs
/// on the runtime. The two primitives that did exist could not express it:
/// `send_to_all` cannot name a key at all (which is precisely how a per-tx
/// assignment got broadcast away), and the blocking variant would park a tokio
/// worker thread for up to `send_timeout` per hop.
///
/// Returns the keys that did NOT receive the payload, for the same reason the
/// blocking variant does: a target that is not in the bucket has no known rhash,
/// so a failure against it cannot be attributed to the delivery-failure counter
/// and can only be reported to the caller — which, for ADR-004, is what turns
/// "the assigned Finalizer was unreachable" into a failed transaction instead of
/// a transaction that silently never finalizes.
///
/// A target naming this host's own key is served through the inbound path (see
/// [`Self::deliver_self_copy`]) and never enters the peer loop — the bucket
/// cannot hold the local entry, so the bucket lookup would report it as
/// undelivered on the exact host that can always serve it.
pub async fn send_to_peers(
    &self,
    keys: &[Vec<u8>],
    node_type: &NodeRegistryType,
    data: &[u8],
) -> Vec<Vec<u8>> {
    let nodes = match self.get_nodes(node_type) {
        Some(nodes) => nodes,
        None => {
            // No bucket for the type. A target naming our own key can still be
            // served locally; every other target is undelivered rather than the
            // call quietly succeeding.
            let own_key = self.config.public_key.clone();
            return keys
                .iter()
                .filter(|key| !(key.as_slice() == own_key.as_slice()
                    && self.deliver_self_copy(data, node_type)))
                .cloned()
                .collect();
        }
    };

    // Resolve targets up front so no DashMap guard is live across a send
    // (same discipline as `send_to_all`). `None` rhash = not in the bucket.
    let targets: Vec<(Vec<u8>, Option<[u8; 16]>)> = keys
        .iter()
        .map(|key| (key.clone(), nodes.get(key).map(|entry| entry.value().rhash)))
        .collect();

    let failures = Arc::clone(&self.delivery_failures);
    let own_key = self.config.public_key.clone();
    let peers: Vec<(Vec<u8>, Option<[u8; 16]>)> = targets
        .into_iter()
        .filter(|(key, rhash)| {
            rhash.is_some() || key != &own_key || !self.deliver_self_copy(data, node_type)
        })
        .collect();

    let mut undelivered: Vec<Vec<u8>> = Vec::new();

    if let Some(network) = &self.network {
        let futs = peers.into_iter().filter_map(|(key, rhash)| {
            let Some(rhash) = rhash else {
                undelivered.push(key);
                return None;
            };
            let network = Arc::clone(network);
            let node_type = node_type.clone();
            let failures = Arc::clone(&failures);
            let send_data = data.to_vec();
            Some(async move {
                match bounded_send_async(self.send_timeout, move || {
                    let _ = RnsSender::new(network, rhash).get_response(&send_data);
                })
                .await
                {
                    Ok(()) => None,
                    Err(e) => {
                        record_delivery_failure(&failures, rhash, &node_type, e);
                        Some(key)
                    }
                }
            })
        });
        for missed in join_all(futs).await {
            if let Some(key) = missed {
                undelivered.push(key);
            }
        }
        return undelivered;
    }

    // Direct branch: same concurrency as `send_to_all`, restricted to the named
    // keys. A key evicted between resolution and send reports as undelivered
    // instead of counting as a delivery that never happened.
    let futs = peers.into_iter().map(|(key, rhash)| {
        let node_type = node_type.clone();
        let failures = Arc::clone(&failures);
        let nodes_clone = Arc::clone(&nodes);
        let send_data = data.to_vec();
        async move {
            let Some(entry) = nodes_clone.get(&key) else {
                return Some(key);
            };
            let rhash = entry.value().rhash;
            match tokio::time::timeout(self.send_timeout, entry.value().conn.send(&send_data)).await
            {
                Ok(Ok(())) => None,
                Ok(Err(e)) => {
                    record_delivery_failure(&failures, rhash, &node_type, e);
                    Some(key)
                }
                Err(_) => {
                    record_delivery_failure(
                        &failures,
                        rhash,
                        &node_type,
                        ConnError::Timeout(format!(
                            "direct send exceeded {:?}",
                            self.send_timeout
                        )),
                    );
                    Some(key)
                }
            }
        }
    });
    for missed in join_all(futs).await {
        if let Some(key) = missed {
            undelivered.push(key);
        }
    }
    undelivered
}

/// Blocking version for sync contexts (runs sends sequentially). Each send
/// is bounded by `self.send_timeout` and runs on a detached std thread, with no
/// ambient runtime assumed (H7): a hung RNS route or socket degrades to
/// `Err(ConnError::Timeout)` instead of hanging the caller. Every failed
/// delivery is recorded + logged (Phase 6.2).
pub fn send_to_all_blocking(&self, data: Vec<u8>, node_type: &NodeRegistryType) {
    // Same-host hop first — see `send_to_all` and `self_delivery`.
    self.deliver_self_copy(&data, node_type);

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
            match nodes_for_send.get(&key) {
                Some(entry) => {
                    tokio::time::timeout(self.send_timeout, entry.value().conn.send(&send_data)).await
                }
                // Evicted between collection and send. Reporting `Ok(Ok(()))`
                // here counted a delivery that never happened — the same
                // silence-as-emptiness the Phase 6.2 counters exist to remove.
                // Wrapped as `Ok(Err(..))` to match `timeout`'s inner result.
                None => Ok(Err(ConnError::IO(
                    "peer removed from the bucket before the send".to_string(),
                ))),
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

/// Targeted fan-out to a specific set of peers, with the SAME delivery-failure
/// accounting as [`Self::send_to_all`].
///
/// This exists because the per-key sends it replaces bypassed the counters
/// completely: they spawned an OS thread and a fresh `current_thread` runtime
/// per key per transaction, discarded every send result, and returned success
/// even when the target was not in the bucket at all. Mesh fragments report
/// `delivery_failures`, so a shard preload or a single-finalizer request that
/// reached nobody made the cluster look *healthier* than a dense one while
/// silently dropping work — silence read as emptiness, in the direction that
/// hides the fault.
///
/// Cost scales with the call rather than with the number of targets: one thread
/// and one runtime serve the whole batch.
///
/// Returns the targets that did NOT receive the payload. Send failures are also
/// recorded in the counter, because a registered peer has an rhash to attribute
/// them to; a target that is not in the bucket has no known rhash, so it can
/// only be reported here — which is why returning the list is part of the API
/// rather than just logging.
pub fn send_to_peers_blocking(
    &self,
    keys: &[Vec<u8>],
    node_type: &NodeRegistryType,
    data: &[u8],
) -> Vec<Vec<u8>> {
    let Some(nodes) = self.get_nodes(node_type) else {
        // The bucket itself is missing: nobody can be reached, so every
        // target counts as undelivered rather than the call quietly succeeding.
        return keys.to_vec();
    };

    // Resolve targets up front so no DashMap guard is live while a send runs
    // (same discipline as send_to_all). `None` rhash = not in the bucket.
    let targets: Vec<(Vec<u8>, Option<[u8; 16]>)> = keys
        .iter()
        .map(|key| {
            let rhash = nodes.get(key).map(|entry| entry.value().rhash);
            (key.clone(), rhash)
        })
        .collect();

    let failures = Arc::clone(&self.delivery_failures);
    let mut undelivered: Vec<Vec<u8>> = Vec::new();

    // A target naming this host's own key is a same-host hop. The bucket cannot
    // hold it — peering registers peers and directory sync refuses our own entry
    // — so an unresolved self-target used to be reported as undelivered, which
    // on a composite is the common case rather than the rare one: the sentinel
    // that selects its own host's finalizer (`request_single_finalizer`, and the
    // shard-preload variant) was failing a transaction it could have served
    // itself. Deliver it through the inbound path, and only fall through to the
    // peer machinery for everyone else.
    let own_key = self.config.public_key.clone();
    let peers: Vec<(Vec<u8>, Option<[u8; 16]>)> = targets
        .into_iter()
        .filter(|(key, rhash)| {
            rhash.is_some() || key != &own_key || !self.deliver_self_copy(data, node_type)
        })
        .collect();

    if let Some(network) = &self.network {
        for (key, rhash) in peers {
            let Some(rhash) = rhash else {
                undelivered.push(key);
                continue;
            };
            let network = Arc::clone(network);
            let send_data = data.to_vec();
            match bounded_send(self.send_timeout, move || {
                let _ = RnsSender::new(network, rhash).get_response(&send_data);
            }) {
                Ok(()) => {}
                Err(e) => {
                    record_delivery_failure(&failures, rhash, node_type, e);
                    undelivered.push(key);
                }
            }
        }
        return undelivered;
    }

    // Direct branch: one self-contained runtime for the whole batch. A
    // `current_thread` runtime is reused across `block_on` calls, so this is
    // one thread per send *site* rather than one per key.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build local runtime for targeted send");

    for (key, rhash) in peers {
        let Some(rhash) = rhash else {
            undelivered.push(key);
            continue;
        };
        let nodes_for_send = Arc::clone(&nodes);
        let send_key = key.clone();
        let send_data = data.to_vec();
        let result = runtime.block_on(async move {
            match nodes_for_send.get(&send_key) {
                Some(entry) => {
                    tokio::time::timeout(self.send_timeout, entry.value().conn.send(&send_data)).await
                }
                // Evicted between resolution and send: a failure, not a no-op.
                // `Ok(Err(..))` to match `timeout`'s inner result type.
                None => Ok(Err(ConnError::IO(
                    "peer removed from the bucket before the send".to_string(),
                ))),
            }
        });
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                record_delivery_failure(&failures, rhash, node_type, e);
                undelivered.push(key);
            }
            Err(_) => {
                record_delivery_failure(
                    &failures,
                    rhash,
                    node_type,
                    ConnError::Timeout(format!("direct send exceeded {:?}", self.send_timeout)),
                );
                undelivered.push(key);
            }
        }
    }
    undelivered
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
