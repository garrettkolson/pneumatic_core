use std::io::Error;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::net::UnixListener;
use crate::conns::streams::{CoreTcpStream, CoreUdsStream, Stream};

pub trait Listener {
    fn accept(&self) -> Result<(Box<dyn Stream>, SocketAddr), std::io::Error>;
}

pub struct CoreUdsListener {
    inner_listener: UnixListener
}

impl CoreUdsListener {
    pub(crate) fn new(addr: std::os::unix::net::SocketAddr) -> Self {
        CoreUdsListener {
            inner_listener: UnixListener::bind_addr(&addr)
                .expect(&format!("Couldn't set up internal UDS listener on socket {0}", addr.as_pathname()))
        }
    }
}

impl Listener for CoreUdsListener {
    fn accept(&self) -> Result<(Box<dyn Stream>, SocketAddr), Error> {
        let (stream, addr) = self.inner_listener.accept()?;
        let core_stream = Box::new(CoreUdsStream::from_stream(stream));
        Ok((core_stream, addr))
    }
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