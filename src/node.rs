use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

pub enum NodeType {
    Full,
    Light
}

pub struct NodeTypeConfig {
    pub min: usize,
    pub max: usize,
    pub min_stake: u64
}

#[derive(Serialize, Deserialize)]
pub enum NodeRequestType {
    Register,
    Request,
    Heartbeat
}

#[derive(Serialize, Deserialize)]
pub enum InternalRegistrationBatch {
    Add(Vec<InternalRegistration>),
    Remove(Vec<InternalRegistration>)
}

#[derive(Serialize, Deserialize)]
pub struct InternalRegistration {
    pub node_key: Vec<u8>,
    pub ip_addr: Option<IpAddr>,
    pub node_types: Vec<NodeRegistryType>
}

impl InternalRegistration {
    pub fn for_add(key: Vec<u8>, addr: IpAddr, types: Vec<NodeRegistryType>) -> Self {
        InternalRegistration {
            node_key: key,
            ip_addr: Some(addr),
            node_types: types
        }
    }

    pub fn for_removal(key: Vec<u8>, types: Vec<NodeRegistryType>) -> Self {
        InternalRegistration {
            node_key: key,
            ip_addr: None,
            node_types: types
        }
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
    Archiver
}

#[derive(Serialize, Deserialize)]
pub struct NodeRequest {
    pub requester_key: Vec<u8>,
    pub requester_ip: IpAddr,
    pub request_type: NodeRequestType,
    pub requester_types: Vec<NodeRegistryType>,
    pub requested_type: NodeRegistryType
}

#[derive(Serialize, Deserialize)]
pub struct NodeRegistryResponse {
    pub responder_key: Vec<u8>,
    pub responder_ip: IpAddr,
    pub registry_type: NodeRegistryType,
    pub entries: Vec<NodeRegistryEntry>
}

#[derive(Serialize, Deserialize)]
pub struct NodeRegistryRequest {
    pub requester_key: Vec<u8>,
    pub requester_ip: Ipv6Addr,
    pub requester_types: Vec<NodeRegistryType>,
    pub requested_type: NodeRegistryType
}

impl NodeRegistryRequest {
    pub fn new(key: Vec<u8>,
               addr: Ipv6Addr,
               requester_types: Vec<NodeRegistryType>,
               requested_type: NodeRegistryType) -> Self {
        NodeRegistryRequest {
            requester_key: key,
            requester_ip: addr,
            requester_types,
            requested_type
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct NodeRegistryEntry {
    pub node_key: Vec<u8>,
    pub node_ip: IpAddr
}

// #[derive(Serialize, Deserialize)]
// pub struct NodeRegistryEntry {
//     pub node_public_key: Vec<u8>,
//     pub public_ip_addr: Ipv6Addr,
//     pub environment_ids: Vec<String>,
//     pub requested_type: NodeRegistryType,
//     pub available_types: Vec<NodeRegistryType>,
//     pub accepted: bool,
//
//     #[serde(skip_serializing, skip_deserializing)]
//     pub connection: Option<Arc<Mutex<Box<dyn crate::conns::IsConnection>>>>
// }
//
// impl NodeRegistryEntry {
//     pub fn new(node_public_key: Vec<u8>,
//                public_ip_addr: Ipv6Addr,
//                requested_type: NodeRegistryType,
//                available_types: Vec<NodeRegistryType>,
//                environment_ids: Vec<String>) -> NodeRegistryEntry {
//         NodeRegistryEntry {
//             node_public_key,
//             public_ip_addr,
//             requested_type,
//             available_types,
//             environment_ids,
//             connection: None,
//             accepted: false
//         }
//     }
// }

#[derive(Debug)]
pub struct NodeBootstrapError {
    pub message: String
}

impl NodeBootstrapError {
    pub fn from_io_error(error: std::io::Error) -> NodeBootstrapError {
        NodeBootstrapError {
            message: error.to_string(),
        }
    }
}