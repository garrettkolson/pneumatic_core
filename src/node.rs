mod registry;

use std::io::Read;
use std::net::{IpAddr};
use std::sync::{Arc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;
use crate::conns::{Connection};
use crate::conns::factories::IsConnFactory;
use crate::conns::streams::Stream;
use crate::data::{DataProvider};

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
pub enum RegistrationBatch {
    Add(Vec<Registration>),
    Remove(Vec<Registration>)
}

#[derive(Serialize, Deserialize)]
pub struct Registration {
    pub node_key: Vec<u8>,
    pub ip_addr: Option<IpAddr>,
    pub node_types: Vec<NodeRegistryType>
}

impl Registration {
    pub fn for_add(key: Vec<u8>, addr: IpAddr, types: Vec<NodeRegistryType>) -> Self {
        Registration {
            node_key: key,
            ip_addr: Some(addr),
            node_types: types
        }
    }

    pub fn for_removal(key: Vec<u8>, types: Vec<NodeRegistryType>) -> Self {
        Registration {
            node_key: key,
            ip_addr: None,
            node_types: types
        }
    }
}

#[derive(Serialize, Deserialize)]
pub enum RegistrationBatchResult {
    Success,
    Failure(NodeRegistrationError)
}

pub type Nodes = Arc<DashMap<Vec<u8>, NodeRegistryNode>>;

pub struct NodeRegistryNode {
    pub ip: IpAddr,
    pub conn: Box<dyn Connection>
}

impl NodeRegistryNode {
    fn new(ip: IpAddr, conn: Box<dyn Connection>) -> Self {
        NodeRegistryNode {
            ip,
            conn
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
    pub requester_ip: IpAddr,
    pub requester_types: Vec<NodeRegistryType>,
    pub requested_type: NodeRegistryType
}

impl NodeRegistryRequest {
    pub fn new(key: Vec<u8>,
               addr: IpAddr,
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

/////////////////// Errors //////////////////////

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

#[derive(Debug)]
#[derive(Serialize, Deserialize)]
pub enum NodeRegistrationError {
    FromUnderlying(String),
    Unknown
}