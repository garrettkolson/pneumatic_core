use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::thread;
use std::thread::JoinHandle;
use dashmap::DashMap;
use crate::config::Config;
use crate::{conns, messages, server};
use crate::conns::ConnTarget;
use crate::conns::factories::IsConnFactory;
use crate::conns::streams::Stream;
use crate::data::DefaultDataProvider;
use crate::encoding::deserialize_rmp_to;
use crate::node::*;
use crate::node::RegistrationBatchResult::Success;
use crate::user::User;

pub struct NodeRegistry {
    committers: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    sentinels: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    executors: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    finalizers: Arc<DashMap<Vec<u8>, NodeRegistryNode>>,
    config: Arc<Config>,
    conn_factory: Arc<Box<dyn IsConnFactory>>,
    on_received: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>
}

impl NodeRegistry {
    const LISTENER_THREAD_COUNT: usize = 4;

    pub fn init(config: Arc<Config>,
                conn_factory: Box<dyn IsConnFactory>,
                on_received: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>)
                -> Self {
        NodeRegistry {
            committers: Arc::new(DashMap::new()),
            sentinels: Arc::new(DashMap::new()),
            executors: Arc::new(DashMap::new()),
            finalizers: Arc::new(DashMap::new()),
            config,
            conn_factory: Arc::new(conn_factory),
            on_received
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

    fn type_is_maxed_out(&self, node_type: &NodeRegistryType) -> bool {
        match self.get_nodes(node_type) {
            Some(nodes) => nodes.len() >= self.config.get_max_node_number(node_type),
            None => true
        }
    }

    pub fn listen_for_conn_requests(registry: Arc<NodeRegistry>, this_func_type: &NodeRegistryType) {
        let port_num = conns::get_internal_port(this_func_type);
        // TODO: replace this addr with the actual public addr of this node
        let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port_num);

        let listener = registry.conn_factory.get_listener(ConnTarget::Remote(addr))
            .expect("Couldn't create listener for incoming connection requests");
        let thread_pool = server::ThreadPool::build(Self::LISTENER_THREAD_COUNT)
            .expect("Couldn't establish thread pool for listening for connection requests");

        loop {
            match listener.accept() {
                Err(_) => continue,
                Ok(mut stream) => {
                    let cloned_registry = registry.clone();
                    let _ = thread_pool.execute_async(Box::pin(async move {
                        let Ok(data) = conns::get_data(&mut stream) else { return };
                        cloned_registry.process_conn_request(data, stream).await
                    }));
                }
            }
        }
    }

    async fn process_conn_request(&self, data: Vec<u8>, mut stream: Box<dyn Stream>) {
        let Ok(request) = deserialize_rmp_to::<NodeRegistryRequest>(&data) else {
            let _ = stream.write_all(&messages::reject());
            return
        };

        let Some(node_type) = self.can_accept_this_connection(&request) else {
            let _ = stream.write_all(&messages::reject());
            return;
        };

        let Some(mut conn) = self.conn_factory.create_connection(stream, self.on_received.clone())
            else { return };

        if conn.send(&messages::acknowledge()).await.is_err() { return; }
        self.register_connection(conn, request, node_type)
    }

    fn register_connection(&self, conn: Box<dyn Connection>,
                           request: NodeRegistryRequest, node_type: NodeRegistryType) {
        let node = NodeRegistryNode::new(request.requester_ip, conn);
        match self.get_nodes(&node_type) {
            None => return,
            Some(nodes) => { nodes.insert(request.requester_key, node); }
        }
    }

    fn can_accept_this_connection(&self, request: &NodeRegistryRequest) -> Option<NodeRegistryType> {
        let node_type = match self.select_registration_node_type(request) {
            None => return None,
            Some(t) => t
        };

        if !Self::check_db_node_user(&request.requester_key,
                                 &self.config.main_environment_id,
                                 self.config.get_min_type_stake(&request.requested_type)) { return None; }

        Some(node_type)
    }

    fn check_db_node_user(user_key: &Vec<u8>, environment_id: &str, min_stake: u64) -> bool {
        let Ok(token) = DefaultDataProvider::new().get_token(user_key, environment_id)
            else { return false };
        let Some(user) = token.get_asset::<User>() else { return false };
        user.fuel_balance > min_stake
    }

    fn select_registration_node_type(&self, request: &NodeRegistryRequest) -> Option<NodeRegistryType> {
        if self.can_select_this_type(request, NodeRegistryType::Finalizer) {
            return Some(NodeRegistryType::Finalizer);
        }

        if self.can_select_this_type(request, NodeRegistryType::Executor) {
            return Some(NodeRegistryType::Executor);
        }

        if self.can_select_this_type(request, NodeRegistryType::Sentinel) {
            return Some(NodeRegistryType::Sentinel);
        }

        if self.can_select_this_type(request, NodeRegistryType::Committer) {
            return Some(NodeRegistryType::Committer);
        }

        None
    }

    fn can_select_this_type(&self, request: &NodeRegistryRequest, node_type: NodeRegistryType) -> bool {
        request.requester_types.contains(&node_type) && !self.type_is_maxed_out(&node_type)
    }

    pub fn send_to_all(&self, data: Vec<u8>, node_type: &NodeRegistryType) {
        // TODO: have to redo this to use registered conns instead of senders
        let shared_data = Arc::new(RwLock::new(data));
        let Some(nodes) = self.get_nodes(node_type) else { return };

        let threads: Vec<JoinHandle<Vec<u8>>> = nodes.iter().map(|node| -> JoinHandle<Vec<u8>> {
            let data_clone = shared_data.clone();
            let addr = SocketAddr::new(node.value().ip.clone(), conns::get_external_port(node_type));
            let sender_result = self.conn_factory.get_sender(ConnTarget::Remote(addr));
            thread::spawn(move || {
                if let Ok(sender) = sender_result {
                    let Ok(read_data) = data_clone.read() else { return vec![] };
                    sender.get_response(read_data.as_slice()).unwrap_or_else(|_| vec![])
                }
                else {
                    vec![]
                }
            })
        }).collect();

        for thread in threads {
            let _ = thread.join();
        }
    }

    fn process_registration(&self, registration_batch: RegistrationBatch) -> RegistrationBatchResult {
        Success
    }
}