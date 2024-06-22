use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};

pub trait IsConnection : Send {

}

pub trait FireAndForgetSender {
    fn send(&self, addr: impl ToSocketAddrs, data: &[u8]);
}

pub struct TcpFafSender {

}

impl FireAndForgetSender for TcpFafSender {
    fn send(&self, addr: impl ToSocketAddrs, data: &[u8]) {
        if let Ok(mut stream) = TcpStream::connect(addr) {
            let _ = stream.write_all(data);
        }
    }
}

pub trait ConnFactory {
    fn get_faf_sender(&self) -> impl FireAndForgetSender;
}

pub struct TcpConnFactory {

}

impl ConnFactory for TcpConnFactory {
    fn get_faf_sender(&self) -> TcpFafSender {
        TcpFafSender {}
    }
}

pub const HEARTBEAT_PORT: u16 = 44000;
pub const COMMITTER_PORT: u16 = 45000;
pub const SENTINEL_PORT: u16 = 46000;
pub const EXECUTOR_PORT: u16 = 47000;
pub const FINALIZER_PORT: u16 = 48000;