use std::io::Write;
use std::net::{SocketAddrV4, SocketAddrV6, TcpStream};

pub trait IsConnection : Send {

}

pub trait FireAndForgetSender {
    fn send_to_v4(&self, addr: SocketAddrV4, data: &[u8]);
    fn send_to_v6(&self, addr: SocketAddrV6, data: &[u8]);
}

pub struct TcpFafSender {

}

impl FireAndForgetSender for TcpFafSender {
    fn send_to_v4(&self, addr: SocketAddrV4, data: &[u8]) {
        if let Ok(mut stream) = TcpStream::connect(addr) {
            let _ = stream.write_all(data);
        }    }

    fn send_to_v6(&self, addr: SocketAddrV6, data: &[u8]) {
        if let Ok(mut stream) = TcpStream::connect(addr) {
            let _ = stream.write_all(data);
        }
    }
}

pub trait ConnFactory {
    fn get_faf_sender(&self) -> Box<dyn FireAndForgetSender>;
}

pub struct TcpConnFactory {

}

impl TcpConnFactory {
    pub fn new() -> TcpConnFactory {
        TcpConnFactory {}
    }
}

impl ConnFactory for TcpConnFactory {
    fn get_faf_sender(&self) -> Box<dyn FireAndForgetSender> {
        Box::new(TcpFafSender {})
    }
}

pub const HEARTBEAT_PORT: u16 = 44000;
pub const COMMITTER_PORT: u16 = 45000;
pub const SENTINEL_PORT: u16 = 46000;
pub const EXECUTOR_PORT: u16 = 47000;
pub const FINALIZER_PORT: u16 = 48000;