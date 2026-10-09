//! Same-host delivery for `NodeRegistry`'s fan-out — the composite's
//! self-subscription.
//!
//! Every pipeline hop is a fan-out to a *role* bucket, and a bucket holds
//! **peers only**: peering registers peers, and `handle_directory_response`
//! refuses to install our own entry ("a node is not its own peer", so a
//! directory can never put our rhash in our own directories). On a four-role
//! composite host that is a hole with the host's own shape — a role fanning
//! out to a type this same machine runs talks to everyone but itself, so the
//! message a node sends to its own role never arrives. `fact-composite-no-self-delivery`
//! has the arithmetic (three peers' `Commit` copies arrive, the one copy whose
//! block hash matches the local booking never does, chain growth is
//! mathematically impossible).
//!
//! The seam is the symmetric half of the inbound bridge: the composite already
//! routes every inbound frame through one closure (`on_packet` →
//! `route_data_plane` → `RoleDispatcher`), so a local copy is fed through
//! **that same closure** as the frame bytes the wire would have carried. Same
//! parse, same directory-response branch, same admit-lists, same dispatcher,
//! same handler — a local copy is not a privileged path.
//!
//! Installed by the composite bridge and by nothing else: a single-role host
//! (the standalone committer, a split-deployment sentinel) never receives a
//! copy of its own broadcast, which is exactly how it behaved before this
//! module existed.

use super::*;

/// The inbound path a local copy is fed into: the byte shape an RNS worker
/// hands the application handler — a serialized [`NetworkPacket`] frame, not a
/// bare `Message`.
pub type PacketSink = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

/// What this host owes itself when a fan-out targets a role it runs.
pub struct SelfDelivery {
    /// The role types installed on this host. A fan-out to a type outside this
    /// list owes nothing locally — that is the non-composite case, where the
    /// host is simply not a member of the target set.
    roles: Vec<NodeRegistryType>,
    /// The same closure the RNS bridge installs as its packet handler.
    sink: PacketSink,
}

impl SelfDelivery {
    pub fn serves(&self, node_type: &NodeRegistryType) -> bool {
        self.roles.iter().any(|t| t == node_type)
    }
}

impl NodeRegistry {
    /// Install the host's self-subscription: "this machine runs `roles`, and
    /// here is its inbound path". Called by the composite bridge after it
    /// installs role plugins; nothing else calls it, so a single-role host keeps
    /// the peer-only behaviour of a split deployment.
    pub fn install_self_delivery(&self, roles: Vec<NodeRegistryType>, sink: PacketSink) {
        *self.self_delivery.write().unwrap() = Some(Arc::new(SelfDelivery { roles, sink }));
    }

    /// The roles the installed self-subscription covers — empty when none was
    /// installed. Test/measurement accessor: it is how a test proves the bridge
    /// wired the subscription to the roles it actually installed, rather than
    /// to the wider set the node's config declares.
    pub fn self_delivery_roles(&self) -> Vec<NodeRegistryType> {
        self.self_delivery
            .read()
            .unwrap()
            .as_ref()
            .map(|sd| sd.roles.clone())
            .unwrap_or_default()
    }

    /// Does this host run `node_type` itself? True only where a
    /// self-subscription was installed — role-bucket emptiness is *not* the
    /// same question on a composite (a lone composite host has a finalizer; it
    /// simply is not in its own bucket). Callers that refuse to fan out on an
    /// empty bucket consult this so a same-host role is not counted as absent.
    pub fn serves_locally(&self, node_type: &NodeRegistryType) -> bool {
        self.self_delivery
            .read()
            .unwrap()
            .as_ref()
            .map(|sd| sd.serves(node_type))
            .unwrap_or(false)
    }

    /// Deliver the local copy a fan-out to `node_type` owes this host, framed
    /// exactly as the wire would frame it. Returns `true` when a copy went out.
    ///
    /// Exactly-once rests on two guards, both structural:
    ///
    /// - the host must *run* the target role (otherwise it is not a member of
    ///   the target set and owes itself nothing);
    /// - the host must **not** already be in the target bucket. Peering and
    ///   directory sync never put it there in production, but a fixture that
    ///   registers the node's own key as a peer (the composite e2e relay
    ///   convention) would otherwise get the message twice — once over its
    ///   recording connection and once locally.
    ///
    /// The sink itself cannot report failure: it is the inbound path, which
    /// surfaces its own errors at its own level (dispatch failures and control
    /// errors are logged there, exactly as a wire frame's would be). A frame
    /// that cannot even be serialized is a delivery failure and is counted as
    /// one, against this host's own rhash.
    pub(crate) fn deliver_self_copy(&self, data: &[u8], node_type: &NodeRegistryType) -> bool {
        let Some(subscription) = self.self_delivery.read().unwrap().clone() else {
            return false;
        };
        if !subscription.serves(node_type) {
            return false;
        }
        let already_in_the_bucket = self
            .get_nodes(node_type)
            .map(|nodes| nodes.contains_key(&self.config.public_key))
            .unwrap_or(false);
        if already_in_the_bucket {
            return false;
        }

        let frame = NetworkPacket {
            control: None,
            data: Some(data.to_vec()),
        };
        let frame_bytes = match serialize_to_bytes_rmp(&frame) {
            Ok(bytes) => bytes,
            Err(e) => {
                record_delivery_failure(
                    &self.delivery_failures,
                    self.config.rhash,
                    node_type,
                    ConnError::IO(format!("self-delivery frame serialize: {e}")),
                );
                return false;
            }
        };
        (subscription.sink)(frame_bytes);
        true
    }
}
