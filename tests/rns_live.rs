//! Live-network integration test for RNS transport.
//!
//! This test requires a running Reticulum network. It is marked with
//! `#[ignore]` so it doesn't run in CI by default. To run it:
//! ```bash
//! cargo test --test rns_live -- --ignored
//! ```

use pneumatic_core::rns::wrapper::RnsNetwork;
use pneumatic_core::rns::identity::NodeIdentity;

#[tokio::test]
#[ignore]
async fn test_rns_live_delivery() {
    // Generate a local identity
    let identity = NodeIdentity::generate_in_memory();

    // TODO: Configure RnsNode to connect to the test Reticulum network
    // This requires:
    // 1. NodeConfig with the network's bootstrap servers
    // 2. Starting the RnsNetwork
    // 3. Announcing our identity
    // 4. Waiting for the network to connect

    // For now, this test is a placeholder that will fail if run.
    // The full implementation requires a running Reticulum network.
    assert!(false, "Live network test requires a running Reticulum network");
}
