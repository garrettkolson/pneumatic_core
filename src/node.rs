use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

#[derive(Serialize, Deserialize)]
pub enum NodeRequestType {
    Register,
    Request,
    Heartbeat
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
    pub node_public_key: Vec<u8>,
    pub public_ip_addr: Ipv6Addr,
    pub environment_ids: Vec<String>,
    pub requested_type: NodeRegistryType,
    pub available_types: Vec<NodeRegistryType>,
    pub accepted: bool,

    #[serde(skip_serializing, skip_deserializing)]
    pub connection: Option<Arc<Mutex<Box<dyn crate::conns::IsConnection>>>>
}

impl NodeRegistryEntry {
    pub fn new(node_public_key: Vec<u8>,
               public_ip_addr: Ipv6Addr,
               requested_type: NodeRegistryType,
               available_types: Vec<NodeRegistryType>,
               environment_ids: Vec<String>) -> NodeRegistryEntry {
        NodeRegistryEntry {
            node_public_key,
            public_ip_addr,
            requested_type,
            available_types,
            environment_ids,
            connection: None,
            accepted: false
        }
    }
}