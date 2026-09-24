//! Heartbeat handling for `NodeRegistry`: authenticated `last_seen`
//! refresh (forged heartbeats are dropped).

use super::*;

impl NodeRegistry {
pub(crate) fn handle_heartbeat(&self, request: &NodeRequest) {
    // Authenticate the binding signature over the claimed
    // `(requester_rhash, requested_type, requester_types)` before trusting
    // the key — without this any sender can refresh a registered node's
    // liveness with a forged heartbeat, making `last_seen` forgeable
    // (Phase 1.6, L1). Fail closed: reject before the registered-key
    // lookup so `refresh_last_seen` is never reached for a forged request.
    if !NodeIdentity::verify_binding(
        &request.requester_key,
        &request.requester_rhash,
        &request.requested_type,
        &request.requester_types,
        &request.binding_signature,
    ) {
        return;
    }

    if let Some(existing_type) = self.find_node_type_by_public_key(&request.requester_key) {
        self.refresh_last_seen(&request.requester_key, &existing_type);
    }
}

pub(crate) fn refresh_last_seen(&self, key: &Vec<u8>, node_type: &NodeRegistryType) {
    if let Some(nodes) = self.get_nodes(node_type) {
        if let Some(mut entry) = nodes.get_mut(key) {
            entry.value_mut().last_seen = Instant::now();
        }
    }
}
}
