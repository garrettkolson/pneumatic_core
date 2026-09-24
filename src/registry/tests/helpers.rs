//! Shared fixtures for the registry test suite (committer convention:
//! cross-file fixtures live here, pub-ified, re-exporting the module
//! header's test-only types).

pub use std::sync::Arc;
pub use std::thread;

use super::super::*;


// --- Shielded transfers (Phase S5.1.2) ---

/// A minimal shielded tx for registry tests (dummy field contents).
pub fn shielded_fixture(id: &str) -> ShieldedTransaction {
    ShieldedTransaction {
        id: id.into(),
        action: "ShieldedTransfer".into(),
        token_id: vec![1, 2, 3],
        spent_commitments: vec![[6u8; 32]],
        nullifiers: vec![[1u8; 32]],
        commitments: vec![[3u8; 32]],
        merkle_root: [5u8; 32],
        proof: vec![9, 8, 7, 6],
        note_ciphertexts: vec![vec![10u8; 64]],
        fee: 0,
    }
}
