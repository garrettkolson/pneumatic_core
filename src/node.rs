pub mod registry;

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

use crate::conns::Connection;

#[derive(Clone)]
pub enum NodeType {
    Full,
    Light,
}

#[derive(Clone)]
pub struct NodeTypeConfig {
    pub min: usize,
    pub max: usize,
    pub min_stake: u64,
}

/// Control-plane request types carried in `NodeRequest`.
#[derive(Serialize, Deserialize)]
pub enum NodeRequestType {
    Register,
    Request,
    Heartbeat,
    /// Reply to a `Register`. `responder_key` is mandatory on accept: it is
    /// the responder's Ed25519 public key, which the requester stores in its
    /// own directory. `reason` is empty on accept and explains the rejection
    /// otherwise.
    RegisterAck {
        accepted: bool,
        node_type: NodeRegistryType,
        responder_key: Vec<u8>,
        reason: String,
    },
}

#[derive(Serialize, Deserialize)]
pub enum RegistrationBatch {
    Add(Vec<Registration>),
    Remove(Vec<Registration>),
}

#[derive(Serialize, Deserialize)]
pub struct Registration {
    pub node_key: Vec<u8>,
    /// Transport address (rhash) of the registered node.
    pub rhash: [u8; 16],
    pub node_types: Vec<NodeRegistryType>,
}

impl Registration {
    pub fn for_add(key: Vec<u8>, rhash: [u8; 16], types: Vec<NodeRegistryType>) -> Self {
        Registration {
            node_key: key,
            rhash,
            node_types: types,
        }
    }

    pub fn for_removal(key: Vec<u8>, rhash: [u8; 16], types: Vec<NodeRegistryType>) -> Self {
        Registration {
            node_key: key,
            rhash,
            node_types: types,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub enum RegistrationBatchResult {
    Success,
    Failure(NodeRegistrationError),
}

pub type Nodes = Arc<DashMap<Vec<u8>, NodeRegistryNode>>;

pub struct NodeRegistryNode {
    /// Transport address (rhash) of the connected node.
    pub rhash: [u8; 16],
    pub conn: Box<dyn Connection>,
    /// Last time any inbound packet was seen from this node (heartbeat/eviction).
    pub last_seen: Instant,
    /// Binding this node produced at registration, so we can vouch for it in
    /// directory responses. Empty ⇒ we cannot vouch for this node (it was not
    /// directly registered here), and it is skipped when building directory
    /// entries. See `NodeIdentity::sign_binding`.
    pub directory_signature: Vec<u8>,
    /// `requested_type` from the node's own registration binding — needed to
    /// re-verify `directory_signature` with `verify_binding` when we list it.
    pub directory_requested_type: NodeRegistryType,
    /// The node's role set from its registration binding.
    pub directory_node_types: Vec<NodeRegistryType>,
}

impl NodeRegistryNode {
    /// A plain connection with no directory binding (nodes learned via a
    /// directory response or seeded in tests). Such a node cannot be vouched
    /// for by us, so it is skipped when building directory entries.
    pub fn new(rhash: [u8; 16], conn: Box<dyn Connection>) -> Self {
        NodeRegistryNode {
            rhash,
            conn,
            last_seen: Instant::now(),
            directory_signature: Vec::new(),
            directory_requested_type: NodeRegistryType::Archiver,
            directory_node_types: Vec::new(),
        }
    }

    /// A node that registered directly, carrying its own rhash binding so the
    /// directory can echo it in responses and peers can verify it.
    pub fn with_binding(
        rhash: [u8; 16],
        conn: Box<dyn Connection>,
        directory_signature: Vec<u8>,
        directory_requested_type: NodeRegistryType,
        directory_node_types: Vec<NodeRegistryType>,
    ) -> Self {
        let mut node = NodeRegistryNode::new(rhash, conn);
        node.directory_signature = directory_signature;
        node.directory_requested_type = directory_requested_type;
        node.directory_node_types = directory_node_types;
        node
    }
}

#[derive(Eq)]
#[derive(PartialEq)]
#[derive(Hash)]
#[derive(Clone)]
#[derive(Serialize, Deserialize)]
#[derive(Debug, EnumIter)]
pub enum NodeRegistryType {
    Committer,
    Sentinel,
    Executor,
    Finalizer,
    Archiver,
}

/// A control-plane request.
///
/// RNS is destination-encrypted: the transport guarantees the packet was
/// addressed to *us* (only our RNS key can decrypt it) and ratchets
/// authenticate the immediate link neighbor, but multi-hop paths hide the
/// original sender and rns-net's delivery callback exposes no sender
/// identity. So the sender's transport address is *claimed* in the payload
/// — `requester_rhash` — and bound to the Ed25519 on-chain key
/// (`requester_key`) by `binding_signature`, an Ed25519 signature over
/// `(requester_rhash, requested_type, requester_types)`. Forging the claim
/// requires the victim's Ed25519 private key, and actually receiving data
/// addressed to the claimed rhash requires the victim's RNS private key.
#[derive(Serialize, Deserialize)]
pub struct NodeRequest {
    pub requester_key: Vec<u8>,
    /// Claimed transport address (rhash) of the sender; covered by the
    /// binding signature.
    pub requester_rhash: [u8; 16],
    pub request_type: NodeRequestType,
    pub requester_types: Vec<NodeRegistryType>,
    pub requested_type: NodeRegistryType,
    pub binding_signature: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
pub struct NodeRegistryResponse {
    pub responder_key: Vec<u8>,
    pub responder_rhash: [u8; 16],
    pub registry_type: NodeRegistryType,
    pub entries: Vec<NodeRegistryEntry>,
    /// Ed25519 signature over the rmp serialization of
    /// `(entries, registry_type, responder_rhash)`, made by the responder.
    /// The envelope proves the responder vouched for this exact set of
    /// entries under this type and rhash; each `entry.signature` independently
    /// proves the *listed* node bound its own rhash (Phase 1.5). Directory
    /// entries are hints, not authority — the receiver re-checks both.
    pub signature: Vec<u8>,
}

/// Legacy data-plane registration request (sentinel). Kept for the
/// existing sentinel handler; registration is migrating to the control
/// plane (`NodeRequest` + `RegisterAck`).
#[derive(Serialize, Deserialize)]
pub struct NodeRegistryRequest {
    pub requester_key: Vec<u8>,
    /// Transport address (rhash) of the requester, authenticated by the
    /// RNS transport.
    pub rhash: [u8; 16],
    pub requester_types: Vec<NodeRegistryType>,
    pub requested_type: NodeRegistryType,
    pub binding_signature: Vec<u8>,
}

impl NodeRegistryRequest {
    pub fn new(
        key: Vec<u8>,
        rhash: [u8; 16],
        binding_signature: Vec<u8>,
        requester_types: Vec<NodeRegistryType>,
        requested_type: NodeRegistryType,
    ) -> Self {
        NodeRegistryRequest {
            requester_key: key,
            rhash,
            binding_signature,
            requester_types,
            requested_type,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct NodeRegistryEntry {
    pub node_key: Vec<u8>,
    pub node_rhash: [u8; 16],
    /// The listed node's own binding signature over its rhash, produced when
    /// it registered. Lets the receiver authenticate `(node_key, node_rhash)`
    /// independently of the responder — without it a directory could attribute
    /// a fabricated rhash to a node it never saw bind it (Phase 1.5, C7).
    /// This is the signature, and `requested_type` + `node_types` are the
    /// remaining fields of the `(rhash, requested_type, requester_types)`
    /// tuple that `NodeIdentity::verify_binding` re-checks.
    pub signature: Vec<u8>,
    pub requested_type: NodeRegistryType,
    pub node_types: Vec<NodeRegistryType>,
}

/// Top-level wire packet from the RNS data plane: either a control-plane
/// `NodeRequest` or raw data-plane message bytes. The explicit split
/// removes the old dual-deserialize guessing.
#[derive(Serialize, Deserialize)]
pub struct NetworkPacket {
    pub control: Option<NodeRequest>,
    pub data: Option<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};

    #[test]
    fn node_request_register_round_trip() {
        let req = NodeRequest {
            requester_key: vec![1, 2, 3],
            requester_rhash: [9u8; 16],
            request_type: NodeRequestType::Register,
            requester_types: vec![NodeRegistryType::Committer, NodeRegistryType::Finalizer],
            requested_type: NodeRegistryType::Committer,
            binding_signature: vec![7, 8, 9],
        };
        let bytes = serialize_to_bytes_rmp(&req).unwrap();
        let back: NodeRequest = deserialize_rmp_to(&bytes).unwrap();
        assert_eq!(back.requester_key, vec![1, 2, 3]);
        assert_eq!(back.requester_rhash, [9u8; 16]);
        assert_eq!(back.requester_types, req.requester_types);
        assert_eq!(back.requested_type, NodeRegistryType::Committer);
        assert_eq!(back.binding_signature, vec![7, 8, 9]);
        assert!(matches!(back.request_type, NodeRequestType::Register));
    }

    #[test]
    fn node_request_register_ack_round_trip() {
        let req = NodeRequest {
            requester_key: vec![1, 2],
            requester_rhash: [4u8; 16],
            request_type: NodeRequestType::RegisterAck {
                accepted: true,
                node_type: NodeRegistryType::Finalizer,
                responder_key: vec![3, 4, 5],
                reason: String::new(),
            },
            requester_types: vec![NodeRegistryType::Finalizer],
            requested_type: NodeRegistryType::Finalizer,
            binding_signature: vec![6],
        };
        let bytes = serialize_to_bytes_rmp(&req).unwrap();
        let back: NodeRequest = deserialize_rmp_to(&bytes).unwrap();
        let NodeRequestType::RegisterAck {
            accepted,
            node_type,
            responder_key,
            reason,
        } = back.request_type
        else {
            panic!("expected RegisterAck");
        };
        assert!(accepted);
        assert_eq!(node_type, NodeRegistryType::Finalizer);
        assert_eq!(responder_key, vec![3, 4, 5]);
        assert!(reason.is_empty());
        assert_eq!(back.requester_rhash, [4u8; 16]);
        assert_eq!(back.requester_key, vec![1, 2]);
    }

    #[test]
    fn node_registry_request_round_trip() {
        let req = NodeRegistryRequest::new(
            vec![1, 2, 3, 4],
            [5u8; 16],
            vec![9, 9],
            vec![NodeRegistryType::Sentinel],
            NodeRegistryType::Sentinel,
        );
        let bytes = serialize_to_bytes_rmp(&req).unwrap();
        let back: NodeRegistryRequest = deserialize_rmp_to(&bytes).unwrap();
        assert_eq!(back.requester_key, vec![1, 2, 3, 4]);
        assert_eq!(back.rhash, [5u8; 16]);
        assert_eq!(back.requester_types, vec![NodeRegistryType::Sentinel]);
        assert_eq!(back.requested_type, NodeRegistryType::Sentinel);
        assert_eq!(back.binding_signature, vec![9, 9]);
    }

    #[test]
    fn node_registry_response_round_trip() {
        let resp = NodeRegistryResponse {
            responder_key: vec![1],
            responder_rhash: [2u8; 16],
            registry_type: NodeRegistryType::Committer,
            entries: vec![
                NodeRegistryEntry {
                    node_key: vec![10],
                    node_rhash: [11u8; 16],
                    signature: vec![1, 1],
                    requested_type: NodeRegistryType::Committer,
                    node_types: vec![NodeRegistryType::Committer],
                },
                NodeRegistryEntry {
                    node_key: vec![12],
                    node_rhash: [13u8; 16],
                    signature: vec![2, 2],
                    requested_type: NodeRegistryType::Executor,
                    node_types: vec![NodeRegistryType::Executor],
                },
            ],
            signature: vec![4, 4],
        };
        let bytes = serialize_to_bytes_rmp(&resp).unwrap();
        let back: NodeRegistryResponse = deserialize_rmp_to(&bytes).unwrap();
        assert_eq!(back.responder_rhash, [2u8; 16]);
        assert_eq!(back.registry_type, NodeRegistryType::Committer);
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.entries[0].node_key, vec![10]);
        assert_eq!(back.entries[0].node_rhash, [11u8; 16]);
        assert_eq!(back.entries[0].signature, vec![1, 1]);
        assert_eq!(back.entries[0].requested_type, NodeRegistryType::Committer);
        assert_eq!(back.entries[1].node_rhash, [13u8; 16]);
        assert_eq!(back.entries[1].requested_type, NodeRegistryType::Executor);
        assert_eq!(back.signature, vec![4, 4]);
    }

    #[test]
    fn registration_batch_round_trip() {
        let reg = Registration::for_add(vec![1], [2u8; 16], vec![NodeRegistryType::Executor]);
        let reg2 = Registration::for_add(vec![1], [2u8; 16], vec![NodeRegistryType::Executor]);
        for batch in [RegistrationBatch::Add(vec![reg]), RegistrationBatch::Remove(vec![reg2])] {
            let bytes = serialize_to_bytes_rmp(&batch).unwrap();
            let back: RegistrationBatch = deserialize_rmp_to(&bytes).unwrap();
            match back {
                RegistrationBatch::Add(es) | RegistrationBatch::Remove(es) => {
                    assert_eq!(es.len(), 1);
                    assert_eq!(es[0].node_key, vec![1]);
                    assert_eq!(es[0].rhash, [2u8; 16]);
                    assert_eq!(es[0].node_types, vec![NodeRegistryType::Executor]);
                }
            }
        }
    }

    #[test]
    fn network_packet_round_trip() {
        // Control-only
        let req = NodeRequest {
            requester_key: vec![1],
            requester_rhash: [3u8; 16],
            request_type: NodeRequestType::Heartbeat,
            requester_types: vec![NodeRegistryType::Committer],
            requested_type: NodeRegistryType::Committer,
            binding_signature: vec![],
        };
        let req2 = NodeRequest {
            requester_key: vec![1],
            requester_rhash: [3u8; 16],
            request_type: NodeRequestType::Heartbeat,
            requester_types: vec![NodeRegistryType::Committer],
            requested_type: NodeRegistryType::Committer,
            binding_signature: vec![],
        };
        let bytes = serialize_to_bytes_rmp(&NetworkPacket { control: Some(req2), data: None }).unwrap();
        let back: NetworkPacket = deserialize_rmp_to(&bytes).unwrap();
        assert!(back.data.is_none());
        let control = back.control.expect("control packet lost control");
        assert_eq!(control.requester_rhash, [3u8; 16]);
        assert!(matches!(control.request_type, NodeRequestType::Heartbeat));

        // Data-only
        let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let bytes = serialize_to_bytes_rmp(&NetworkPacket { control: None, data: Some(payload.clone()) }).unwrap();
        let back: NetworkPacket = deserialize_rmp_to(&bytes).unwrap();
        assert!(back.control.is_none());
        assert_eq!(back.data, Some(payload));

        // Both (defensive: the wire format allows it)
        let bytes = serialize_to_bytes_rmp(&NetworkPacket { control: Some(req), data: Some(vec![7]) }).unwrap();
        let back: NetworkPacket = deserialize_rmp_to(&bytes).unwrap();
        assert!(back.control.is_some());
        assert_eq!(back.data, Some(vec![7]));
    }
}

/////////////////// Errors //////////////////////

#[derive(Debug)]
pub struct NodeBootstrapError {
    pub message: String,
}

impl NodeBootstrapError {
    pub fn from_io_error(error: std::io::Error) -> NodeBootstrapError {
        NodeBootstrapError {
            message: error.to_string(),
        }
    }
}

#[derive(Debug)]
#[derive(Serialize, Deserialize)]
pub enum NodeRegistrationError {
    FromUnderlying(String),
    Unknown,
}
