use std::path::PathBuf;
use std::string::ToString;
use std::sync::Arc;
use std::time::Duration;

use crate::conns::listeners::{CoreTcpListener, CoreUdsListener, Listener};
use crate::conns::senders::{TcpSender, UdsSender};
use crate::conns::streams::Stream;
use crate::conns::uds::data_socket_path;
use crate::conns::{Connection, ConnError, ConnTarget, LocalTarget, Sender, TcpConnection};

const NOT_UNIX_MESSAGE: &str =
    "This is not a Unix runtime environment. Use the TCP loopback address for internal communication.";

/// Default blocking read/write bound applied to factory-built senders so a hung
/// data service can't wedge a caller thread (H7).
const DEFAULT_RW_TIMEOUT: Duration = Duration::from_secs(15);

pub trait IsConnFactory: Send + Sync {
    fn get_sender(&self, target: ConnTarget) -> Result<Box<dyn Sender>, ConnError>;
    fn get_listener(&self, target: ConnTarget) -> Result<Box<dyn Listener>, ConnError>;
    fn create_connection(&self,
                         stream: Box<dyn Stream>,
                         on_received: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>)
                         -> Option<Box<dyn Connection>>;
}

pub struct ConnFactory {
    /// Shared secret for HMAC-authenticated data-channel framing. `None` keeps
    /// the unauthenticated (legacy / test) path.
    shared_secret: Option<Vec<u8>>,
    /// Blocking read/write bound applied to factory-built senders so a hung
    /// data service can't wedge a caller thread (H7).
    rw_timeout: Duration,
}

impl ConnFactory {
    pub fn new() -> ConnFactory {
        ConnFactory { shared_secret: None, rw_timeout: DEFAULT_RW_TIMEOUT }
    }

    /// Set the shared secret used to frame/auth all factory-built senders.
    pub fn with_secret(mut self, shared_secret: Option<Vec<u8>>) -> ConnFactory {
        self.shared_secret = shared_secret;
        self
    }

    /// Override the blocking read/write bound applied to factory-built senders.
    pub fn with_timeout(mut self, rw_timeout: Duration) -> ConnFactory {
        self.rw_timeout = rw_timeout;
        self
    }
}

impl Default for ConnFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl IsConnFactory for ConnFactory {
    fn get_sender(&self, target: ConnTarget) -> Result<Box<dyn Sender>, ConnError> {
        let sender: Box<dyn Sender> = match target {
            ConnTarget::Remote(addr) => Box::new(
                TcpSender::new(addr, self.shared_secret.clone())
                    .with_timeouts(Some(self.rw_timeout), Some(self.rw_timeout))),
            ConnTarget::Local(local) => match local {
                LocalTarget::Tcp(addr) => Box::new(
                    TcpSender::new(addr, self.shared_secret.clone())
                        .with_timeouts(Some(self.rw_timeout), Some(self.rw_timeout))),
                LocalTarget::Unix(path) => {
                    if !cfg!(unix) {
                        return Err(ConnError::MalformedData(NOT_UNIX_MESSAGE.to_string()));
                    }
                    Box::new(
                        UdsSender::new(path, self.shared_secret.clone())
                            .with_timeouts(Some(self.rw_timeout), Some(self.rw_timeout)))
                }
            },
        };
        Ok(sender)
    }

    fn get_listener(&self, target: ConnTarget) -> Result<Box<dyn Listener>, ConnError> {
        let listener: Box<dyn Listener> = match target {
            ConnTarget::Remote(addr) => Box::new(CoreTcpListener::new(addr)?),
            ConnTarget::Local(local) => match local {
                LocalTarget::Tcp(addr) => Box::new(CoreTcpListener::new(addr)?),
                LocalTarget::Unix(location) => {
                    if !cfg!(unix) {
                        return Err(ConnError::MalformedData(NOT_UNIX_MESSAGE.to_string()));
                    }
                    // Absolute, per-UID path with symlink/stale handling.
                    let path: PathBuf = data_socket_path(&location)
                        .map_err(|e| ConnError::IO(e.to_string()))?;
                    Box::new(CoreUdsListener::new(&path)?)
                }
            },
        };
        Ok(listener)
    }

    fn create_connection(&self,
                         stream: Box<dyn Stream>,
                         on_received: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>)
                                -> Option<Box<dyn Connection>> {
        match TcpConnection::from_stream(stream, on_received) {
            Ok(conn) => Some(Box::new(conn)),
            Err(_) => None
        }
    }
}
