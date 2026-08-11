pub mod streams;
pub mod factories;
pub mod senders;
pub mod listeners;

use std::fmt::{Debug, Display, Formatter};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use crate::conns::senders::Sender;
use crate::conns::streams::{Stream, StreamReader, StreamWriter};
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

pub fn get_data(reader: &mut Box<dyn Stream>) -> Result<Vec<u8>, ConnError> {
    let mut header: Vec<u8> = vec![0u8, 4];
    if let Err(err) = reader.read_exact(&mut header) {
        return Err(ConnError::ReadError(Some(err.to_string())))
    }

    let data_length = usize::from_be_bytes(header.try_into().unwrap_or_default());
    let mut data: Vec<u8> = vec![0u8; data_length];
    match reader.read_exact(&mut data) {
        Ok(_) => Ok(data),
        Err(err) => Err(ConnError::ReadError(Some(err.to_string())))
    }
}

pub async fn get_data_async(reader: &mut Box<dyn StreamReader>) -> Result<Vec<u8>, ConnError> {
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

#[async_trait]
pub trait Connection : Send + Sync {
    async fn send(&self, data: &Vec<u8>) -> Result<(), ConnError>;
}

struct TcpConnection {
    writer: Option<tokio::sync::Mutex<Box<dyn StreamWriter>>>,
    listening_thread: Option<tokio::task::JoinHandle<()>>,
}

impl TcpConnection {
    pub fn from_stream(stream: Box<dyn Stream>,
                          on_received: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>)
        -> Result<Self, ConnError> {
        let (mut reader, writer) = stream.into_split()?;
        let thread = tokio::spawn(async move {
            loop {
                match get_data_async(&mut reader).await {
                    Ok(data) => on_received(data),
                    Err(ConnError::ReadError(_)) => continue,
                    _ => break
                }
            }
        });

        Ok(TcpConnection {
            writer: Some(tokio::sync::Mutex::new(writer)),
            listening_thread: Some(thread),
        })
    }
}

#[async_trait]
impl Connection for TcpConnection {
    async fn send(&self, data: &Vec<u8>) -> Result<(), ConnError> {
        let length_header = data.len().to_be_bytes();
        let writer = self.writer.as_ref().expect("connection dropped");
        let mut w = writer.lock().await;
        w.write_all(&length_header).await?;
        w.write_all(data).await
    }
}

impl Drop for TcpConnection {
    fn drop(&mut self) {
        // Drop the writer to close the write half of the split stream.
        // This causes the reader in listening_thread to fail with a ReadError,
        // breaking out of the loop and allowing the task to complete.
        drop(self.writer.take());

        // Detach the thread: if the stream was already closed externally,
        // the read loop may not exit. The OS cleans up the thread.
        drop(self.listening_thread.take());
    }
}

pub enum ConnTarget {
    Local(LocalTarget),
    Remote(SocketAddr)
}

impl Clone for ConnTarget {
    fn clone(&self) -> Self {
        match self {
            ConnTarget::Remote(addr) => ConnTarget::Remote(addr.clone()),
            ConnTarget::Local(addr_or_path) => {
                match addr_or_path {
                    LocalTarget::Unix(path) => ConnTarget::Local(LocalTarget::Unix(path.clone())),
                    LocalTarget::Tcp(addr) => ConnTarget::Local(LocalTarget::Tcp(addr.clone()))
                }
            }
        }
    }
}

pub enum LocalTarget {
    Unix(String),
    Tcp(SocketAddr)
}

pub enum ConnError {
    IO(String),
    MalformedData(String),
    CouldNotEstablishStream,
    WriteError(Option<String>),
    ReadError(Option<String>),
    ConnectionRejectedByRemote
}

impl Debug for ConnError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnError::IO(msg) => f.debug_tuple("IO").field(msg).finish(),
            ConnError::MalformedData(msg) => f.debug_tuple("MalformedData").field(msg).finish(),
            ConnError::CouldNotEstablishStream => f.write_str("CouldNotEstablishStream"),
            ConnError::WriteError(msg) => f.debug_tuple("WriteError").field(msg).finish(),
            ConnError::ReadError(msg) => f.debug_tuple("ReadError").field(msg).finish(),
            ConnError::ConnectionRejectedByRemote => {
                f.write_str("ConnectionRejectedByRemote")
            }
        }
    }
}

impl Display for ConnError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnError::IO(msg) => write!(f, "IO({})", msg),
            ConnError::MalformedData(msg) => write!(f, "MalformedData({})", msg),
            ConnError::CouldNotEstablishStream => f.write_str("CouldNotEstablishStream"),
            ConnError::WriteError(msg) => write!(f, "WriteError({:?})", msg),
            ConnError::ReadError(msg) => write!(f, "ReadError({:?})", msg),
            ConnError::ConnectionRejectedByRemote => {
                f.write_str("ConnectionRejectedByRemote")
            }
        }
    }
}

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
