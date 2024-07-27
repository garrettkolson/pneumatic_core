use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;
use crate::conns::ConnError;

const CONN_TIMEOUT_IN_SECS: u64 = 60;

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