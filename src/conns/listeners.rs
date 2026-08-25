use std::io::Error;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::net::UnixListener;
use std::path::Path;

use crate::conns::streams::{CoreTcpStream, CoreUdsStream, Stream};
use crate::conns::uds::prepare_socket_path;
use crate::conns::ConnError;

pub trait Listener {
    fn accept(&self) -> Result<Box<dyn Stream>, std::io::Error>;
}

pub struct CoreUdsListener {
    inner_listener: UnixListener,
}

impl CoreUdsListener {
    /// Bind a UDS listener, preparing the path first: refuse a pre-created
    /// symlink (bind hijack) and remove a stale regular socket left behind by a
    /// crashed listener. Returns `Result` instead of panicking on bind failure.
    pub(crate) fn new(path: &Path) -> Result<Self, ConnError> {
        prepare_socket_path(path)?;
        let inner_listener = UnixListener::bind(path)
            .map_err(|e| ConnError::IO(e.to_string()))?;
        Ok(CoreUdsListener { inner_listener })
    }
}

impl Listener for CoreUdsListener {
    fn accept(&self) -> Result<Box<dyn Stream>, Error> {
        let (stream, _addr) = self.inner_listener.accept()?;
        let core_stream = Box::new(CoreUdsStream::from_stream(stream));
        Ok(core_stream)
    }
}

pub struct CoreTcpListener {
    inner_listener: TcpListener,
}

impl CoreTcpListener {
    /// Bind a TCP listener, returning `Result` instead of panicking on failure.
    pub(crate) fn new(addr: SocketAddr) -> Result<Self, ConnError> {
        let inner_listener = TcpListener::bind(addr)
            .map_err(|e| ConnError::IO(e.to_string()))?;
        Ok(CoreTcpListener { inner_listener })
    }
}

impl Listener for CoreTcpListener {
    fn accept(&self) -> Result<Box<dyn Stream>, std::io::Error> {
        let (stream, _addr) = self.inner_listener.accept()?;
        let core_stream = Box::new(CoreTcpStream::from_stream(stream));
        Ok(core_stream)
    }
}
