//! Node registration for the Sentinel (stake-gated Register requests).
//!
//! These are `impl Sentinel` methods, so they retain access to the struct's
//! private fields (child modules of `crate::sentinel`).

use super::*;

impl Sentinel {
    /// Handle a "Register" request — a node registering with this sentinel.
    pub(crate) fn handle_register_request(&self, message: Message) -> Result<(), SentinelError> {
        let request: NodeRegistryRequest = deserialize_rmp_to(&message.body)
            .map_err(|e| SentinelError::Encoding(e))?;

        // The binding signature authenticates that the Ed25519 key is bound
        // to the claimed rhash — the same check the control-plane registry
        // applies to `NodeRequest` Register messages.
        if !pneumatic_core::rns::identity::NodeIdentity::verify_binding(
            &request.requester_key,
            &request.rhash,
            &request.requested_type,
            &request.requester_types,
            &request.binding_signature,
        ) {
            return Err(SentinelError::Registry(
                "invalid binding signature".to_string(),
            ));
        }

        // Reject if already registered for the requested type.
        if self.node_registry.node_is_already_registered(&request.requester_key, &request.requested_type) {
            return Err(SentinelError::Registry(format!(
                "Node {:?} already registered as {:?}",
                request.requester_key, request.requested_type
            )));
        }

        // Validate stake for the requested type.
        if !self.check_stake_for_type(&request.requester_key, &request.requested_type)? {
            return Err(SentinelError::Registry(format!(
                "Insufficient stake for node {:?} registering as {:?}",
                request.requester_key, request.requested_type
            )));
        }

        // Add node to each requested type's registry.
        for node_type in &request.requester_types {
            if let Some(nodes) = self.node_registry.get_nodes(node_type) {
                let node_entry = pneumatic_core::node::NodeRegistryNode::new(
                    request.rhash,
                    Box::new(pneumatic_core::node::registry::NullConnection),
                );
                nodes.insert(request.requester_key.clone(), node_entry);
            }
        }

        Ok(())
    }

    /// Check if the user with the given key has sufficient stake for the requested node type.
    ///
    /// Enforces the same AND semantics as the off-thread registration gate
    /// (`node/stake_index.rs`) and `ActionRouter::check_stake`: the stake must
    /// meet *both* the protocol-level global minimum and the per-type minimum.
    /// A stake below either floor fails closed. See AUDIT Phase 4.4 (H7/H8).
    pub(crate) fn check_stake_for_type(&self, key: &Vec<u8>, node_type: &NodeRegistryType) -> Result<bool, SentinelError> {
        let user = self.data_provider.get_user(key, &self.env_data.environment_id)?;
        let node_cfg = self.node_registry.get_config();
        let global_min = node_cfg.get_global_min_stake();
        let type_min = node_cfg.get_min_type_stake(node_type);
        Ok(meets_minimum_stake(user.stake, global_min, type_min))
    }

}
