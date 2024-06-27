use std::net::IpAddr;

pub trait IsConfiguration {
    fn is_for_testing(&self) -> bool;
}

pub struct Config {
    our_ip_addr: IpAddr
}