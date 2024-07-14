use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use crate::node::NodeRegistryType;

pub fn get_internal_port(node_type: &NodeRegistryType) -> u16 {
    match node_type {
        NodeRegistryType::Committer => COMMITTER_PORT_INTERNAL,
        NodeRegistryType::Archiver => COMMITTER_PORT_INTERNAL,
        NodeRegistryType::Sentinel => SENTINEL_PORT_INTERNAL,
        NodeRegistryType::Executor => EXECUTOR_PORT_INTERNAL,
        NodeRegistryType::Finalizer => FINALIZER_PORT_INTERNAL
    }
}

pub fn get_external_port(node_type: &NodeRegistryType) -> u16 {
    match node_type {
        NodeRegistryType::Committer => COMMITTER_PORT,
        NodeRegistryType::Archiver => COMMITTER_PORT,
        NodeRegistryType::Sentinel => SENTINEL_PORT,
        NodeRegistryType::Executor => EXECUTOR_PORT,
        NodeRegistryType::Finalizer => FINALIZER_PORT
    }
}

pub fn send_on_thread(cloned_data: Arc<RwLock<Vec<u8>>>, conn: Box<dyn Sender>, addr: SocketAddr)
                      -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        match cloned_data.read() {
            Err(_) => vec![],
            Ok(read_data) => {
                conn.get_response(addr, &read_data)
                    .unwrap_or_else(|_| vec![])
            }
        }
    })
}

///////////////////// Senders ////////////////////////

pub trait FireAndForgetSender : Send + Sync {
    fn send(&self, addr: SocketAddr, data: &[u8]);
}

pub struct TcpFafSender { }

impl FireAndForgetSender for TcpFafSender {
    fn send(&self, addr: SocketAddr, data: &[u8]) {
        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(CONN_TIMEOUT_IN_SECS)) {
            let _ = stream.write_all(data);
        }
    }
}

pub trait Sender: Send + Sync {
    fn get_response(&self, addr: SocketAddr, data: &[u8]) -> Result<Vec<u8>, ConnError>;
}

pub struct TcpSender { }

impl Sender for TcpSender {
    fn get_response(&self, addr: SocketAddr, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(CONN_TIMEOUT_IN_SECS)) {
            Ok(stream) => stream,
            Err(err) => return Err(ConnError::IO(err.to_string()))
        };

        if let Err(err) = stream.write_all(data) {
            return Err(ConnError::IO(err.to_string()))
        }

        let mut buffer = Vec::new();
        match stream.read_to_end(&mut buffer) {
            Err(err) => Err(ConnError::IO(err.to_string())),
            Ok(_) => Ok(buffer)
        }
    }
}

///////////////////// Listeners ////////////////////////

pub trait Listener {
    fn accept(&self) -> Result<(Box<dyn Stream>, SocketAddr), std::io::Error>;
}

pub struct CoreTcpListener {
    inner_listener: TcpListener
}

impl CoreTcpListener {
    fn new(addr: SocketAddr) -> Self {
        CoreTcpListener {
            inner_listener: TcpListener::bind(addr)
                .expect(&format!("Couldn't set up external TCP listener on socket {0}", addr))
        }
    }
}

impl Listener for CoreTcpListener {
    fn accept(&self) -> Result<(Box<dyn Stream>, SocketAddr), std::io::Error> {
        let (stream, addr) = self.inner_listener.accept()?;
        let core_stream = Box::new(CoreTcpStream::from_stream(stream));
        Ok((core_stream, addr))
    }
}

///////////////////// Streams ///////////////////////

pub trait Stream : Send + Sync {
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, std::io::Error>;
    fn write_all(&mut self, data: &Vec<u8>) -> Result<(), std::io::Error>;
}

pub struct CoreTcpStream {
    inner_stream: TcpStream
}

impl CoreTcpStream {
    fn from_stream(stream: TcpStream) -> Self {
        CoreTcpStream {
            inner_stream: stream
        }
    }
}

impl Stream for CoreTcpStream {
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, std::io::Error> {
        self.inner_stream.read_to_end(buffer)
    }

    fn write_all(&mut self, data: &Vec<u8>) -> Result<(), std::io::Error> {
        self.inner_stream.write_all(data)
    }
}

////////////////////// Factories ////////////////////////

pub trait ConnFactory : Send + Sync {
    fn get_sender(&self) -> Box<dyn Sender>;
    fn get_faf_sender(&self) -> Box<dyn FireAndForgetSender>;
    fn get_listener(&self, addr: SocketAddr) -> Box<dyn Listener>;
}

pub struct TcpConnFactory { }

impl TcpConnFactory {
    pub fn new() -> TcpConnFactory {
        TcpConnFactory {}
    }
}

impl ConnFactory for TcpConnFactory {
    fn get_sender(&self) -> Box<dyn Sender> {
        Box::new(TcpSender {})
    }

    fn get_faf_sender(&self) -> Box<dyn FireAndForgetSender> {
        Box::new(TcpFafSender {})
    }

    fn get_listener(&self, addr: SocketAddr) -> Box<dyn Listener> {
        Box::new(CoreTcpListener::new(addr))
    }
}

pub enum ConnError {
    IO(String),
    MalformedData(String)
}

const CONN_TIMEOUT_IN_SECS: u64 = 60;

pub const HEARTBEAT_PORT: u16 = 42000;
pub const COMMITTER_PORT: u16 = 42001;
pub const SENTINEL_PORT: u16 = 42002;
pub const EXECUTOR_PORT: u16 = 42003;
pub const FINALIZER_PORT: u16 = 42004;
pub const BEACON_PORT: u16 = 42005;

const COMMITTER_PORT_INTERNAL: u16 = 50000;
const SENTINEL_PORT_INTERNAL: u16 = 50001;
const EXECUTOR_PORT_INTERNAL: u16 = 50002;
const FINALIZER_PORT_INTERNAL: u16 = 50003;