use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};

pub trait IsConnection : Send {

}

pub trait FireAndForgetSender {
    fn send<T>(&self, addr: T, data: Vec<u8>) where T : ToSocketAddrs;
}

pub struct TcpFafSender {

}

impl FireAndForgetSender for TcpFafSender {
    fn send<T>(&self, addr: T, data: Vec<u8>) where T: ToSocketAddrs {
        if let Ok(mut stream) = TcpStream::connect(addr) {
            let _ = stream.write_all(&*data);
        }
    }
}

pub trait ConnFactory {
    fn get_faf_sender() -> dyn FireAndForgetSender;
}

pub struct TcpConnFactory {

}

impl ConnFactory for TcpConnFactory {
    fn get_faf_sender() -> TcpFafSender {
        TcpFafSender {}
    }
}

pub const HEARTBEAT_PORT: u16 = 44000;
pub const COMMITTER_PORT: u16 = 45000;
pub const SENTINEL_PORT: u16 = 46000;
pub const EXECUTOR_PORT: u16 = 47000;
pub const FINALIZER_PORT: u16 = 48000;