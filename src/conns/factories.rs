use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use async_trait::async_trait;
use crate::conns::{Connection, ConnError, Sender, TcpConnection};
use crate::conns::listeners::{CoreTcpListener, Listener};
use crate::conns::senders::{FireAndForgetSender, TcpFafSender, TcpSender};
use crate::conns::streams::{CoreTcpStream, Stream};
use crate::messages::acknowledge;

#[async_trait]
pub trait ConnFactory : Send + Sync {
    fn get_sender(&self) -> Box<dyn Sender>;
    fn get_faf_sender(&self) -> Box<dyn FireAndForgetSender>;
    fn get_listener(&self, addr: SocketAddr) -> Box<dyn Listener>;
    fn create_connection(&self,
                         stream: Box<dyn Stream>,
                         on_received: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>)
                         -> Option<Box<dyn Connection>>;
    async fn request_connection(&self,
                                addr: SocketAddr,
                                on_received: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>)
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