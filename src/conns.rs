use std::io::{Read, Write};
use std::net::{SocketAddrV4, SocketAddrV6, TcpStream};
use crate::encoding;

pub trait IsConnection : Send {

}

pub trait FireAndForgetSender : Send + Sync {
    fn send_to_v4(&self, addr: SocketAddrV4, data: &[u8]);
    fn send_to_v6(&self, addr: SocketAddrV6, data: &[u8]);
}

pub struct TcpFafSender { }

impl FireAndForgetSender for TcpFafSender {
    fn send_to_v4(&self, addr: SocketAddrV4, data: &[u8]) {
        if let Ok(mut stream) = TcpStream::connect(addr) {
            let _ = stream.write_all(data);
        }
    }

    fn send_to_v6(&self, addr: SocketAddrV6, data: &[u8]) {
        if let Ok(mut stream) = TcpStream::connect(addr) {
            let _ = stream.write_all(data);
        }
    }
}

pub trait Sender: Send + Sync {
    fn get_response_from_v4(&self, addr: SocketAddrV4, data: &[u8]) -> Result<Vec<u8>, ConnError>;
    fn get_response_from_v6(&self, addr: SocketAddrV6, data: &[u8]) -> Result<Vec<u8>, ConnError>;
}

pub struct TcpSender { }

impl Sender for TcpSender {
    fn get_response_from_v4(&self, addr: SocketAddrV4, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        let mut stream = match TcpStream::connect(addr) {
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

    fn get_response_from_v6(&self, addr: SocketAddrV6, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        let mut stream = match TcpStream::connect(addr) {
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

pub trait ConnFactory : Send + Sync {
    fn get_sender(&self) -> Box<dyn Sender>;
    fn get_faf_sender(&self) -> Box<dyn FireAndForgetSender>;
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
}

pub enum ConnError {
    IO(String),
    MalformedData(String)
}

pub const HEARTBEAT_PORT: u16 = 44000;
pub const COMMITTER_PORT: u16 = 45000;
pub const SENTINEL_PORT: u16 = 46000;
pub const EXECUTOR_PORT: u16 = 47000;
pub const FINALIZER_PORT: u16 = 48000;