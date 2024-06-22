pub trait IsConnection : Send {

}

const COMMITTER_PORT: u16 = 45000;
const SENTINEL_PORT: u16 = 46000;
const EXECUTOR_PORT: u16 = 47000;
const FINALIZER_PORT: u16 = 48000;