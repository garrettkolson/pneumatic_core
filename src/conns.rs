use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use crate::messages::acknowledge;
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

///////////////////// Connections ///////////////////////

#[async_trait]
pub trait Connection : Send + Sync {
    async fn send(&mut self, data: &Vec<u8>) -> Result<(), ConnError>;
}

pub struct TcpConnection {
    writer: OwnedWriteHalf,
    listening_thread: tokio::task::JoinHandle<()>
}

impl TcpConnection {
    pub fn from_stream<F>(stream: tokio::net::TcpStream, mut on_received: F) -> Self
        where F : FnMut(Vec<u8>) + Send + 'static {
        let (mut reader, mut writer) = stream.into_split();
        let thread = tokio::spawn(async move {
            loop {
                match Self::get_data(&mut reader).await {
                    Ok(data) => on_received(data),
                    Err(ConnError::ReadError(_)) => continue,
                    _ => break
                }
            }

            // TODO: else case should break loop, retry original connection?, then initiate drop
        });

        TcpConnection {
            writer,
            listening_thread: thread,
        }
    }

    async fn get_data(reader: &mut OwnedReadHalf) -> Result<Vec<u8>, ConnError> {
        let mut header: Vec<u8> = vec![0u8, 4];
        if let Err(err) = reader.read_exact(&mut header).await {
            return Err(ConnError::ReadError(Some(err.to_string())))
        }

        let data_length = usize::from_be_bytes(header.try_into().unwrap_or_default());
        let mut data: Vec<u8> = vec![0u8; data_length];
        match reader.read_exact(&mut data).await {
            Ok(_) => Ok(data),
            Err(err) => Err(ConnError::ReadError(Some(err.to_string())))
        }
    }
}

#[async_trait]
impl Connection for TcpConnection {
    async fn send(&mut self, data: &Vec<u8>) -> Result<(), ConnError> {
        let length_header = data.len().to_be_bytes();
        let _ = match self.writer.write_all(&length_header).await {
            Err(err) => Err(ConnError::WriteError(Some(err.to_string()))),
            Ok(()) => Ok(())
        };

        match self.writer.write_all(data).await {
            Ok(()) => Ok(()),
            Err(err) => {
                // TODO: implement flag to drop connection from Registry here
                Err(ConnError::IO(err.to_string()))
            }
        }
    }
}

// impl Drop for TcpConnection {
//     fn drop(&mut self) {
//         self.listening_thread.join();
//     }
// }

//impl Drop for ThreadPool {
//     fn drop(&mut self) {
//         drop(self.sender.take());
//
//         for worker in &mut self.workers {
//             println!("Shutting down worker {}", worker.id);
//             if let Some(thread) = worker.thread.take() {
//                 thread.join().unwrap();
//             }
//         }
//     }
// }

////////////////////// Factories ////////////////////////

#[async_trait]
pub trait ConnFactory : Send + Sync {
    fn get_sender(&self) -> Box<dyn Sender>;
    fn get_faf_sender(&self) -> Box<dyn FireAndForgetSender>;
    fn get_listener(&self, addr: SocketAddr) -> Box<dyn Listener>;
    async fn request_connection(&self, addr: SocketAddr, on_received: Box<dyn FnMut(Vec<u8>) + Send + 'static>)
        -> Result<Box<dyn Connection>, ConnError>;
}

pub struct TcpConnFactory { }

impl TcpConnFactory {
    pub fn new() -> TcpConnFactory {
        TcpConnFactory {}
    }
}

#[async_trait]
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

    async fn request_connection(&self, addr: SocketAddr, on_received: Box<dyn FnMut(Vec<u8>) + Send + 'static>)
        -> Result<Box<dyn Connection>, ConnError> {
        let Ok(mut stream) = tokio::net::TcpStream::connect(addr).await
            else { return Err(ConnError::CouldNotEstablishStream) };

        // TODO: pull function to get this node's registration into nodes module for access
        let request = &vec![];
        let Ok(_) = stream.write_all(request).await
            else { return Err(ConnError::WriteError(None)) };

        let mut data: Vec<u8> = vec![];
        let Ok(_) = stream.read_to_end(&mut data).await
            else { return Err(ConnError::ReadError(None)) };

        match data == acknowledge() {
            false => Err(ConnError::ConnectionRejectedByRemote),
            true => {
                let conn = TcpConnection::from_stream(stream, on_received);
                Ok(Box::new(conn))
            }
        }
    }
}

pub enum ConnError {
    IO(String),
    MalformedData(String),
    CouldNotEstablishStream,
    WriteError(Option<String>),
    ReadError(Option<String>),
    ConnectionRejectedByRemote
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