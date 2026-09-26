//! `Connection` impl over the RNS transport.

use std::sync::Arc;

use crate::conns::{ConnError, Connection};

use super::wrapper::RnsNetwork;

/// A `Connection` addressed to a single peer by rhash. Sends are
/// destination-encrypted RNS packets; routing state is resolved by the
/// network at send time.
pub struct RnsConnection {
    rhash: [u8; 16],
    network: Arc<RnsNetwork>,
}

impl RnsConnection {
    pub fn new(rhash: [u8; 16], network: Arc<RnsNetwork>) -> Self {
        RnsConnection { rhash, network }
    }

    pub fn rhash(&self) -> [u8; 16] {
        self.rhash
    }
}

#[async_trait::async_trait]
impl Connection for RnsConnection {
    async fn send(&self, data: &Vec<u8>) -> Result<(), ConnError> {
        // Data-plane send: wraps the payload in a `NetworkPacket { data }`
        // frame (the wire contract the peer's `on_packet` bridge expects) and
        // routes frames above the ~481 B direct-packet cap through the
        // Resource transfer path — without which every real pneumatic
        // Message (~3.8 KB with the PQC hybrid signature) fails at the RNS
        // pack step (audit 7.1).
        self.network
            .send_data_packet(self.rhash, data)
            .map_err(|e| ConnError::WriteError(Some(e.to_string())))
    }
}
