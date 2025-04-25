use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::string::ToString;
use std::sync::Arc;
use async_trait::async_trait;
use crate::conns::{Connection, ConnError, ConnTarget, LocalTarget, Sender, TcpConnection};
use crate::conns::listeners::{CoreTcpListener, CoreUdsListener, Listener};
use crate::conns::senders::{TcpSender, UdsSender};
use crate::conns::streams::{CoreTcpStream, Stream};
use crate::messages::acknowledge;

const NOT_UNIX_MESSAGE: &str =
    "This is not a Unix runtime environment. Use the TCP loopback address for internal communication.";

#[async_trait]
pub trait IsConnFactory: Send + Sync {
    fn get_sender(&self, target: ConnTarget) -> Result<Box<dyn Sender>, ConnError>;
    fn get_listener(&self, target: ConnTarget) -> Result<Box<dyn Listener>, ConnError>;
    fn create_connection(&self,
                         stream: Box<dyn Stream>,
                         on_received: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>)
                         -> Option<Box<dyn Connection>>;
    async fn request_connection(&self,
                                addr: SocketAddr,
                                on_received: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>)
                                -> Result<Box<dyn Connection>, ConnError>;
}

pub struct ConnFactory {

}

impl ConnFactory {
    pub fn new() -> ConnFactory {
        ConnFactory {}
    }
}

#[async_trait]
impl IsConnFactory for ConnFactory {
    fn get_sender(&self, target: ConnTarget) -> Result<Box<dyn Sender>, ConnError> {
        match target {
            ConnTarget::Remote(addr) => Ok(Box::new(TcpSender::new(addr))),
            ConnTarget::Local(local) => match local {
                LocalTarget::Tcp(addr) => Ok(Box::new(TcpSender::new(addr))),
                LocalTarget::Unix(path) => {
                    if !cfg!(unix) { return Err(ConnError::MalformedData(NOT_UNIX_MESSAGE.to_string())); }
                    Ok(Box::new(UdsSender::new(path)))
                }
            }
        }
    }

    fn get_listener(&self, target: ConnTarget) -> Result<Box<dyn Listener>, ConnError> {
        match target {
            ConnTarget::Remote(addr) => Ok(Box::new(CoreTcpListener::new(addr))),
            ConnTarget::Local(local) => match local {
                LocalTarget::Tcp(addr) => Ok(Box::new(CoreTcpListener::new(addr))),
                LocalTarget::Unix(location) => {
                    if !cfg!(unix) { return Err(ConnError::MalformedData(NOT_UNIX_MESSAGE.to_string())); }
                    let path = format!("/tmp/{}.sock", location);
                    if let Ok(socket) = std::os::unix::net::SocketAddr::from_pathname(path) {
                        Ok(Box::new(CoreUdsListener::new(socket)))
                    }
                    else {
                        Err(ConnError::MalformedData("Bad Unix sockets location".to_string()))
                    }
                }
            }
        }
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

    async fn request_connection(&self,
                                addr: SocketAddr,
                                on_received: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>)
                                -> Result<Box<dyn Connection>, ConnError> {
        let Ok(stream) = TcpStream::connect(addr)
            else { return Err(ConnError::CouldNotEstablishStream) };
        let mut stream: Box<dyn Stream> = Box::new(CoreTcpStream::from_stream(stream));

        // TODO: pull function to get this node's registration into nodes module for access
        let request = &vec![];
        let Ok(_) = stream.write_all(request)
            else { return Err(ConnError::WriteError(None)) };

        let mut data: Vec<u8> = vec![];
        let Ok(_) = stream.read_to_end(&mut data)
            else { return Err(ConnError::ReadError(None)) };

        match data == acknowledge() {
            false => Err(ConnError::ConnectionRejectedByRemote),
            true => {
                let conn = TcpConnection::from_stream(stream, on_received)?;
                Ok(Box::new(conn))
            }
        }
    }
}