pub trait IsConnection : Send {

}

pub const HEARTBEAT_PORT: u16 = 44000;
pub const COMMITTER_PORT: u16 = 45000;
pub const SENTINEL_PORT: u16 = 46000;
pub const EXECUTOR_PORT: u16 = 47000;
pub const FINALIZER_PORT: u16 = 48000;