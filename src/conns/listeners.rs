use std::net::{SocketAddr, TcpListener};
use crate::conns::streams::{CoreTcpStream, Stream};

pub trait Listener {
    fn accept(&self) -> Result<(Box<dyn Stream>, SocketAddr), std::io::Error>;
}

pub struct CoreTcpListener {
    inner_listener: TcpListener
}

impl CoreTcpListener {
    pub(crate) fn new(addr: SocketAddr) -> Self {
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