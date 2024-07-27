pub mod streams;
pub mod factories;
pub mod senders;
pub mod listeners;

use std::fmt::{Display, Formatter};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::thread;
use std::thread::JoinHandle;
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

#[async_trait]
pub trait Connection : Send + Sync {
    async fn send(&mut self, data: &Vec<u8>) -> Result<(), ConnError>;
}

struct TcpConnection {
    writer: Box<dyn StreamWriter>,
    listening_thread: tokio::task::JoinHandle<()>
}

impl TcpConnection {
    pub fn from_stream<F>(stream: Box<dyn Stream>, mut on_received: F) -> Result<Self, ConnError>
        where F : FnMut(Vec<u8>) + Send + 'static {
        let (mut reader, mut writer) = stream.into_split()?;
        let thread = tokio::spawn(async move {
            loop {
                match Self::get_data(&mut reader).await {
                    Ok(data) => on_received(data),
                    Err(ConnError::ReadError(_)) => continue,
                    _ => break
                }
            }

            // TODO: else case should break loop, then initiate drop
        });

        Ok(TcpConnection {
            writer,
            listening_thread: thread,
        })
    }

    async fn get_data(reader: &mut Box<dyn StreamReader>) -> Result<Vec<u8>, ConnError> {
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
        let _ = self.writer.write_all(&length_header).await?;
        self.writer.write_all(data).await
    }
}

// impl Drop for TcpConnection {
//     fn drop(&mut self) {
//         self.listening_thread.join();
//     }
// }

pub enum ConnError {
    IO(String),
    MalformedData(String),
    CouldNotEstablishStream,
    WriteError(Option<String>),
    ReadError(Option<String>),
    ConnectionRejectedByRemote
}

impl Display for ConnError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
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