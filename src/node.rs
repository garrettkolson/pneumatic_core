use std::io::{BufReader, Write};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener};
use std::sync::Arc;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;
use crate::{conns, messages, server};
use crate::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use crate::node::RegistrationBatchResult::Success;

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

pub struct NodeRegistry {
    committers: Arc<DashMap<Vec<u8>, IpAddr>>,
    sentinels: Arc<DashMap<Vec<u8>, IpAddr>>,
    executors: Arc<DashMap<Vec<u8>, IpAddr>>,
    finalizers: Arc<DashMap<Vec<u8>, IpAddr>>,
}

impl NodeRegistry {
    const LISTENER_THREAD_COUNT: usize = 4;

    pub fn init() -> Self {
        NodeRegistry {
            committers: Arc::new(DashMap::new()),
            sentinels: Arc::new(DashMap::new()),
            executors: Arc::new(DashMap::new()),
            finalizers: Arc::new(DashMap::new()),
        }
    }

    pub fn get_nodes(&self, node_type: &NodeRegistryType) -> Option<Nodes> {
        match node_type {
            NodeRegistryType::Committer => Some(Arc::clone(&self.committers)),
            NodeRegistryType::Sentinel => Some(Arc::clone(&self.sentinels)),
            NodeRegistryType::Executor => Some(Arc::clone(&self.executors)),
            NodeRegistryType::Finalizer => Some(Arc::clone(&self.finalizers)),
            _ => None
        }
    }

    pub fn node_is_already_registered(&self, key: &Vec<u8>, node_type: &NodeRegistryType) -> bool {
        match self.get_nodes(node_type) {
            Some(nodes) => nodes.contains_key(key),
            None => false
        }
    }

    pub fn listen_for_updates(registry: Arc<NodeRegistry>, this_func_type: &NodeRegistryType) {
        let port_num = conns::get_internal_port(this_func_type);
        let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port_num);

        let listener = TcpListener::bind(addr)
            .expect("Couldn't set up internal listener for node registry updates");
        let thread_pool = server::ThreadPool::build(Self::LISTENER_THREAD_COUNT)
            .expect("Couldn't establish thread pool for node registry updates");

        // TODO: wrap listener in trait
        for stream in listener.incoming() {
            let _ = match stream {
                Err(_) => continue,
                Ok(mut stream) => {
                    let cloned_registry = registry.clone();
                    let _ = thread_pool.execute(move || {
                        let buf_reader = BufReader::new(&mut stream);
                        let raw_data = buf_reader.buffer().to_vec();
                        let Ok(reg) = deserialize_rmp_to::<RegistrationBatch>(&raw_data)
                            else { return };

                        let _ = cloned_registry.process_registration(reg);
                        stream.write_all(&messages::acknowledge()).unwrap()
                    });
                }
            };
        }
    }

    // TODO: make method to send message to all nodes of type on threads

    fn process_registration(&self, registration_batch: RegistrationBatch) -> RegistrationBatchResult {
        Success
    }
}

pub type Nodes = Arc<DashMap<Vec<u8>, IpAddr>>;

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