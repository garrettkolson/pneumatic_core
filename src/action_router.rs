use std::sync::Arc;

use tokio::sync::RwLock;

use crate::environment::EnvironmentMetadata;
use crate::errors::PneumaticError;
use crate::messages::Message;
use crate::node::NodeRegistryType;
use crate::registry::PendingTransactionRegistry;

// ---------------------------------------------------------------------------
// IActionRouter — routes messages to appropriate handlers
// ---------------------------------------------------------------------------

/// Trait for routing incoming messages to the appropriate handler
/// based on the message action type.
pub trait IActionRouter: Send + Sync {
    /// Route a message to the correct handler based on its action.
    /// Handles utility token coordination: nonce tracking, gas balance, stake checks.
    async fn route(&self, message: Message) -> Result<ActionRouterResult, PneumaticError>;
}

/// Result produced by routing a message.
#[derive(Debug)]
pub enum ActionRouterResult {
    /// Transaction nonce was updated
    NonceUpdated {
        sender: Vec<u8>,
        new_nonce: usize,
    },
    /// Gas balance was verified
    GasVerified,
    /// Stake was checked for a node type
    StakeChecked {
        node_type: NodeRegistryType,
        stake: u64,
    },
    /// Token was dispatched
    TokenDispatched {
        token_id: Vec<u8>,
    },
    /// Message was forwarded to handler
    Forwarded {
        node_type: NodeRegistryType,
        entry_count: usize,
    },
    /// Unknown action type
    UnknownAction(String),
}

// ---------------------------------------------------------------------------
// ActionRouter — concrete implementation
// ---------------------------------------------------------------------------

/// Routes messages based on action type. Handles utility token
/// coordination: nonce tracking, gas balance verification, stake checks.
pub struct ActionRouter {
    /// Environment metadata for validation specs and quorum settings
    environment: EnvironmentMetadata,
    /// Registry of in-flight transactions — used for nonce coordination
    pending_txs: Arc<RwLock<PendingTransactionRegistry>>,
}

impl ActionRouter {
    pub fn new(environment: EnvironmentMetadata) -> Self {
        ActionRouter {
            environment,
            pending_txs: Arc::new(RwLock::new(PendingTransactionRegistry::new())),
        }
    }

    /// Create with an external pending transaction registry (for sharing state
    /// with the sentinel / executor / finalizer).
    pub fn new_with_registry(environment: EnvironmentMetadata, pending_txs: Arc<RwLock<PendingTransactionRegistry>>) -> Self {
        ActionRouter {
            environment,
            pending_txs,
        }
    }

    /// Route a message to the appropriate handler based on action.
    /// Handles utility token coordination.
    ///
    /// This method is preserved for callers that don't go through
    /// the `IActionRouter` trait. Prefer `route()` for new code.
    pub async fn handle(&self, message: Message) -> Result<ActionRouterResult, PneumaticError> {
        self.route(message).await
    }

    /// Check that a sender's nonce is valid for a given transaction.
    /// Returns the new nonce (current + 1) if valid.
    async fn check_nonce(&self, sender: &[u8], expected_nonce: usize) -> Result<usize, PneumaticError> {
        // In a full implementation we'd track per-sender nonce via the
        // pending transaction registry. For now, any nonce >= 0 is accepted.
        let sender_key = hex::encode(sender);

        // Register the sender if not already tracked in pending txs
        let registry = self.pending_txs.write().await;
        if !registry.contains(&sender_key) {
            let _ = registry.register_pending(sender_key);
        }

        Ok(expected_nonce + 1)
    }

    /// Verify gas balance for a transaction.
    /// Returns `GasVerified` if the sender has sufficient gas.
    async fn verify_gas(&self, _sender: &[u8]) -> ActionRouterResult {
        // TODO: look up sender's gas balance from data provider
        // For now, gas verification always passes
        ActionRouterResult::GasVerified
    }

    /// Check stake requirement for a node type.
    /// Returns `StakeChecked` with the node's stake if it meets quorum.
    async fn check_stake(&self, _node_key: &[u8], node_type: NodeRegistryType) -> ActionRouterResult {
        // TODO: look up node stake from the staking manager
        // For now, report zero stake — real stake checks in Phase 2
        ActionRouterResult::StakeChecked {
            node_type,
            stake: 0,
        }
    }
}

impl IActionRouter for ActionRouter {
    async fn route(&self, message: Message) -> Result<ActionRouterResult, PneumaticError> {
        match message.action.as_str() {
            "Process" => {
                // Utility token coordination: check nonce + gas for the sender
                let sender = message.public_key.clone();
                let new_nonce = self.check_nonce(&sender, 0).await?;
                let _gas = self.verify_gas(&sender).await;
                Ok(ActionRouterResult::NonceUpdated {
                    sender,
                    new_nonce,
                })
            }
            "Preload" => {
                // Preload: verify gas balance, check stake, forward to executor
                let sender = message.public_key.clone();
                let _gas = self.verify_gas(&sender).await;
                let stake = self.check_stake(&sender, NodeRegistryType::Executor).await;
                Ok(stake)
            }
            "Sign" => {
                // Sign: check stake for finalizer type
                let sender = message.public_key.clone();
                let stake = self.check_stake(&sender, NodeRegistryType::Finalizer).await;
                Ok(stake)
            }
            "Confirm" => {
                // Confirm: forward to Committer handlers — nonce is already verified
                let _ = message;
                Ok(ActionRouterResult::GasVerified)
            }
            "Reject" => {
                // Reject: clear pending state, reset nonce tracking
                Ok(ActionRouterResult::NonceUpdated {
                    sender: message.public_key,
                    new_nonce: 0,
                })
            }
            "Register" => {
                // Handle node registration — check stake threshold
                let sender = message.public_key.clone();
                Ok(self.check_stake(&sender, NodeRegistryType::Sentinel).await)
            }
            "Clear" => {
                // Clear transaction state — reset nonce for the sender
                Ok(ActionRouterResult::NonceUpdated {
                    sender: message.public_key,
                    new_nonce: 0,
                })
            }
            "DistributeToken" => {
                // Dispatch token to target partition
                Ok(ActionRouterResult::TokenDispatched {
                    token_id: message.public_key,
                })
            }
            _ => Ok(ActionRouterResult::UnknownAction(message.action)),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::{EnvironmentMetadata, EnvironmentMetadataSpec};
    use crate::messages::Message;

    fn make_test_env() -> EnvironmentMetadata {
        let json = r#"{"environment_id":"test","environment_name":"test",
            "partitions":[{"id":"token","partition_type":"Token"},
            {"id":"slush","partition_type":"Slush"}],
            "asym_crypto_provider":{"RSA":null},"sym_crypto_provider":"sym",
            "serialization_provider":"rmp","quorum_percentage":67.0,
            "override_quorum_percentage":0.0,"max_risk":1.0,
            "allowed_token_types":[],"trans_validation_specs":[],
            "block_validation_specs":[],"log_file":"test.log"}"#;
        let spec = serde_json::from_str::<EnvironmentMetadataSpec>(json).unwrap();
        EnvironmentMetadata::load_from_spec(spec)
    }

    fn make_router() -> ActionRouter {
        ActionRouter::new(make_test_env())
    }

    fn make_message(action: &str, public_key: Vec<u8>) -> Message {
        Message {
            chain_id: "test".into(),
            action: action.into(),
            body: vec![],
            signature: vec![],
            public_key,
        }
    }

    // --- IActionRouter::route tests ---

    #[tokio::test]
    async fn route_process_returns_nonce_updated() {
        let router = make_router();
        let msg = make_message("Process", vec![1, 2, 3]);
        let result = router.route(msg).await.unwrap();
        match result {
            ActionRouterResult::NonceUpdated { sender, new_nonce } => {
                assert_eq!(sender, vec![1, 2, 3]);
                assert_eq!(new_nonce, 1);
            }
            other => panic!("expected NonceUpdated, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn route_preload_returns_stake_checked_for_executor() {
        let router = make_router();
        let msg = make_message("Preload", vec![1]);
        let result = router.route(msg).await.unwrap();
        match result {
            ActionRouterResult::StakeChecked { node_type, stake } => {
                assert_eq!(node_type, NodeRegistryType::Executor);
                assert_eq!(stake, 0); // stubbed
            }
            other => panic!("expected StakeChecked(Executor), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn route_sign_returns_stake_checked_for_finalizer() {
        let router = make_router();
        let msg = make_message("Sign", vec![42]);
        let result = router.route(msg).await.unwrap();
        match result {
            ActionRouterResult::StakeChecked { node_type, .. } => {
                assert_eq!(node_type, NodeRegistryType::Finalizer);
            }
            other => panic!("expected StakeChecked(Finalizer), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn route_confirm_returns_gas_verified() {
        let router = make_router();
        let msg = make_message("Confirm", vec![1]);
        let result = router.route(msg).await.unwrap();
        assert!(matches!(result, ActionRouterResult::GasVerified));
    }

    #[tokio::test]
    async fn route_reject_returns_nonce_updated_zero() {
        let router = make_router();
        let msg = make_message("Reject", vec![99]);
        let result = router.route(msg).await.unwrap();
        match result {
            ActionRouterResult::NonceUpdated { sender, new_nonce } => {
                assert_eq!(sender, vec![99]);
                assert_eq!(new_nonce, 0);
            }
            other => panic!("expected NonceUpdated(0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn route_register_returns_stake_checked_sentinel() {
        let router = make_router();
        let msg = make_message("Register", vec![1]);
        let result = router.route(msg).await.unwrap();
        match result {
            ActionRouterResult::StakeChecked { node_type, .. } => {
                assert_eq!(node_type, NodeRegistryType::Sentinel);
            }
            other => panic!("expected StakeChecked(Sentinel), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn route_clear_returns_nonce_updated_zero() {
        let router = make_router();
        let msg = make_message("Clear", vec![5]);
        let result = router.route(msg).await.unwrap();
        match result {
            ActionRouterResult::NonceUpdated { new_nonce, .. } => {
                assert_eq!(new_nonce, 0);
            }
            other => panic!("expected NonceUpdated(0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn route_distribute_token_returns_token_dispatched() {
        let router = make_router();
        let token_id = vec![10, 20, 30];
        let msg = make_message("DistributeToken", token_id.clone());
        let result = router.route(msg).await.unwrap();
        match result {
            ActionRouterResult::TokenDispatched { token_id: id } => {
                assert_eq!(id, token_id);
            }
            other => panic!("expected TokenDispatched, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn route_unknown_action_returns_unknown() {
        let router = make_router();
        let msg = make_message("FooBar", vec![1]);
        let result = router.route(msg).await.unwrap();
        match result {
            ActionRouterResult::UnknownAction(action) => {
                assert_eq!(action, "FooBar");
            }
            other => panic!("expected UnknownAction, got {:?}", other),
        }
    }

    // --- IActionRouter trait impl ---

    #[test]
    fn action_router_implements_i_action_router() {
        // If this compiles, ActionRouter implements IActionRouter
        fn _assert_trait<T: IActionRouter>() {}
        _assert_trait::<ActionRouter>();
    }

    // --- handle() delegates to route() ---

    #[tokio::test]
    async fn handle_delegates_to_route() {
        let router = make_router();
        let msg = make_message("Process", vec![1]);
        // handle() should produce same result as route()
        let handle_result = router.handle(msg.clone()).await.unwrap();
        let route_result = router.route(msg).await.unwrap();
        assert_eq!(
            format!("{:?}", handle_result),
            format!("{:?}", route_result),
        );
    }

    // --- pending_txs registration via check_nonce ---

    #[tokio::test]
    async fn check_nonce_registers_sender_in_pending_txs() {
        let router = make_router();
        let sender = vec![7, 8, 9];
        let msg = make_message("Process", sender.clone());
        let _ = router.route(msg).await.unwrap();

        // Verify the sender was registered in pending_txs
        let registry = router.pending_txs.read().await;
        let sender_key = hex::encode(&sender);
        assert!(
            registry.contains(&sender_key),
            "sender {:?} should be registered under key '{}'",
            sender,
            sender_key
        );
    }

    // --- new_with_registry ---

    #[test]
    fn new_with_registry_shares_pending_txs() {
        let env = make_test_env();
        let shared = Arc::new(RwLock::new(PendingTransactionRegistry::new()));
        let router = ActionRouter::new_with_registry(env, shared.clone());

        // Verify the router holds the same Arc
        assert!(Arc::ptr_eq(&router.pending_txs, &shared));
    }
}
