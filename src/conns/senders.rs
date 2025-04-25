use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;
use crate::conns::{ConnError, ConnTarget};

const CONN_TIMEOUT_IN_SECS: u64 = 60;

pub trait Sender: Send + Sync {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError>;
}

pub(crate) struct UdsSender {
    path: String
}

impl UdsSender {
    pub fn new(target: String) -> Self {
        UdsSender {
            path: target
        }
    }
}

impl Sender for UdsSender {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        let mut stream = match UnixStream::connect(&self.path) {
            Ok(str) => str,
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

pub(crate) struct TcpSender {
    path: SocketAddr
}

impl TcpSender {
    pub fn new(addr: SocketAddr) -> Self {
        TcpSender {
            path: addr
        }
    }
}

impl Sender for TcpSender {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        let mut stream = match TcpStream::connect_timeout(&self.path, Duration::from_secs(CONN_TIMEOUT_IN_SECS)) {
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