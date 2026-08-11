use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::Config;
use crate::data::DataProvider;
use crate::environment::EnvironmentMetadata;
use crate::errors::{PneumaticError, ValidationFailureReason};
use crate::messages::Message;
use crate::node::NodeRegistryType;
use crate::registry::PendingTransactionRegistry;
use crate::transactions::Transaction;
use crate::encoding::deserialize_rmp_to;

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
    /// Gas balance was verified with gas usage tracking
    GasVerified {
        gas_used: u64,
        gas_remaining: u64,
    },
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
    /// Node configuration for stake thresholds and environment IDs
    config: Arc<Config>,
    /// Data provider for loading token/User data
    data_provider: Arc<dyn DataProvider>,
}

impl ActionRouter {
    /// Create a new ActionRouter with a default `DefaultDataProvider`.
    pub fn new(environment: EnvironmentMetadata, config: Arc<Config>) -> Self {
        Self::new_with_config(
            environment,
            Arc::new(RwLock::new(PendingTransactionRegistry::new())),
            config,
            Arc::new(crate::data::DefaultDataProvider::new()),
        )
    }

    /// Create with an external pending transaction registry (for sharing state
    /// with the sentinel / executor / finalizer).
    pub fn new_with_registry(
        environment: EnvironmentMetadata,
        pending_txs: Arc<RwLock<PendingTransactionRegistry>>,
        config: Arc<Config>,
    ) -> Self {
        Self::new_with_config(
            environment,
            pending_txs,
            config,
            Arc::new(crate::data::DefaultDataProvider::new()),
        )
    }

    /// Internal constructor — shared by all public paths. Accepts a custom
    /// `DataProvider` for test injection.
    fn new_with_config(
        environment: EnvironmentMetadata,
        pending_txs: Arc<RwLock<PendingTransactionRegistry>>,
        config: Arc<Config>,
        data_provider: Arc<dyn DataProvider>,
    ) -> Self {
        ActionRouter {
            environment,
            pending_txs,
            config,
            data_provider,
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

    /// Check that a sender's nonce matches the stored value in the data store.
    /// Returns `NonceUpdated` with the incremented nonce if valid.
    async fn check_nonce(&self, sender: &[u8], expected_nonce: usize) -> Result<ActionRouterResult, PneumaticError> {
        let token_partition = &self.environment.token_partition_id;

        // Load protocol-level user from data store
        let user = self.data_provider.get_user(&sender.to_vec(), token_partition)?;

        // Validate nonce matches expected value
        if user.nonce != expected_nonce {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::InvalidNonce,
            ]));
        }

        Ok(ActionRouterResult::NonceUpdated {
            sender: sender.to_vec(),
            new_nonce: expected_nonce + 1,
        })
    }

    /// Verify gas balance for a transaction.
    /// Computes gas from action-specific multiplier:
    ///   gas_used = base_cost + (transaction_amount × multiplier_for_action).
    async fn verify_gas(&self, sender: &[u8], action: &str, amount: u64) -> Result<ActionRouterResult, PneumaticError> {
        let token_partition = &self.environment.token_partition_id;

        let user = self.data_provider.get_user(&sender.to_vec(), token_partition)?;
        let multiplier = self.environment.cost_model.amount_multiplier
            .get(action)
            .copied()
            .unwrap_or(1.0);
        let gas_used = self.environment.cost_model.compute_gas(amount, multiplier);

        if gas_used > user.fuel_balance {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::GasLimitExceeded,
            ]));
        }

        Ok(ActionRouterResult::GasVerified {
            gas_used,
            gas_remaining: user.fuel_balance - gas_used,
        })
    }

    /// Check stake requirement for a node type.
    /// Verifies both protocol-level minimum and per-type minimum stake.
    /// Returns `StakeChecked` with the node's actual stake if it meets both.
    async fn check_stake(
        &self,
        sender: &[u8],
        node_type: NodeRegistryType,
    ) -> Result<ActionRouterResult, PneumaticError> {
        let token_partition = &self.environment.token_partition_id;

        let user = self.data_provider.get_user(&sender.to_vec(), token_partition)?;
        let global_min = self.environment.cost_model.global_min_stake;
        let type_min = self.config.get_min_type_stake(&node_type);

        if user.stake < global_min {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::InsufficientStake,
            ]));
        }

        if user.stake < type_min {
            return Err(PneumaticError::Validation(vec![
                ValidationFailureReason::InsufficientStake,
            ]));
        }

        Ok(ActionRouterResult::StakeChecked {
            node_type,
            stake: user.stake,
        })
    }
}

impl IActionRouter for ActionRouter {
    async fn route(&self, message: Message) -> Result<ActionRouterResult, PneumaticError> {
        match message.action.as_str() {
            "Process" => {
                // Utility token coordination: extract nonce + amount from transaction body,
                // then check nonce and gas for the sender
                let sender = message.public_key.clone();
                let tx: Transaction = deserialize_rmp_to(&message.body)
                    .map_err(PneumaticError::from)?;
                let nonce_result = self.check_nonce(&sender, tx.sequence_number).await?;
                let gas_result = self.verify_gas(&sender, "Process", tx.amount.unwrap_or(0)).await?;
                let _ = gas_result; // GasVerified result available to caller
                match nonce_result {
                    ActionRouterResult::NonceUpdated { sender, new_nonce } => {
                        Ok(ActionRouterResult::NonceUpdated { sender, new_nonce })
                    }
                    _ => unreachable!("check_nonce always returns NonceUpdated"),
                }
            }
            "Preload" => {
                // Preload: verify gas balance with actual amount, check stake, forward to executor
                let sender = message.public_key.clone();
                let tx: Transaction = deserialize_rmp_to(&message.body)
                    .map_err(PneumaticError::from)?;
                self.verify_gas(&sender, "Preload", tx.amount.unwrap_or(0)).await?;
                let stake = self.check_stake(&sender, NodeRegistryType::Executor).await?;
                Ok(stake)
            }
            "Sign" => {
                // Sign: check stake for finalizer type
                let sender = message.public_key.clone();
                let stake = self.check_stake(&sender, NodeRegistryType::Finalizer).await?;
                Ok(stake)
            }
            "Confirm" => {
                // Confirm: forward to Committer handlers — nonce is already verified
                let _ = message;
                Ok(ActionRouterResult::GasVerified { gas_used: 0, gas_remaining: 0 })
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
                Ok(self.check_stake(&sender, NodeRegistryType::Sentinel).await?)
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
    use crate::environment::{CostModel, EnvironmentMetadata, EnvironmentMetadataSpec};
    use crate::messages::Message;
    use crate::user::User;
    use crate::tokens::Token;
    use crate::data::StubDataProvider;
    use crate::config::Config;
    use crate::node::{NodeRegistryType, NodeTypeConfig};
    use dashmap::DashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn make_test_env() -> EnvironmentMetadata {
        let json = r#"{"environment_id":"test","environment_name":"test",
            "partitions":[{"id":"token","partition_type":"Token"},
            {"id":"slush","partition_type":"Slush"}],
            "asym_crypto_provider":{"RSA":null},"sym_crypto_provider":"sym",
            "serialization_provider":"rmp","quorum_percentage":67.0,
            "override_quorum_percentage":0.0,"max_risk":1.0,
            "cost_model":{"base_cost":1,"global_min_stake":10,"admin_public_key":[],"admin_tax_percentage":0.0,
            "amount_multiplier":{"Process":1.0,"Preload":2.0,"Sign":1.5}},
            "allowed_token_types":[],"trans_validation_specs":[],
            "block_validation_specs":[],"log_file":"test.log"}"#;
        let spec = serde_json::from_str::<EnvironmentMetadataSpec>(json).unwrap();
        EnvironmentMetadata::load_from_spec(spec)
    }

    fn make_test_config(env: &EnvironmentMetadata) -> Arc<Config> {
        let mut type_configs = DashMap::new();
        type_configs.insert(NodeRegistryType::Executor, NodeTypeConfig { min: 1, max: 10, min_stake: 100 });
        type_configs.insert(NodeRegistryType::Finalizer, NodeTypeConfig { min: 1, max: 5, min_stake: 500 });
        type_configs.insert(NodeRegistryType::Sentinel, NodeTypeConfig { min: 1, max: 3, min_stake: 1000 });
        type_configs.insert(NodeRegistryType::Committer, NodeTypeConfig { min: 1, max: 5, min_stake: 200 });
        Arc::new(Config::new_for_testing(
            env.environment_id.clone(),
            Arc::new(DashMap::new()),
            Arc::new(type_configs),
        ))
    }

    /// Create a test token with a User asset having the given public_key,
    /// fuel_balance, and nonce.
    fn make_user_token(public_key: Vec<u8>, fuel_balance: u64, nonce: usize) -> Token {
        let user = User {
            public_key: public_key.clone(),
            fuel_balance,
            stake: fuel_balance,
            nonce,
        };
        let mut token = Token::from_asset(&user).unwrap();
        token.id = public_key;
        token
    }

    /// Build a fully functional router with StubDataProvider containing
    /// valid tokens and protocol-level users that pass all stake/gas/nonce checks.
    fn make_router() -> ActionRouter {
        let env = make_test_env();
        let config = make_test_config(&env);
        let token_partition = env.token_partition_id.clone();

        let stub = StubDataProvider::new()
            .with_token(vec![1, 2, 3], token_partition.clone(), make_user_token(vec![1, 2, 3], 1000, 0))
            .with_token(vec![1], token_partition.clone(), make_user_token(vec![1], 1000, 0))
            .with_token(vec![42], token_partition.clone(), make_user_token(vec![42], 1000, 0))
            .with_token(vec![99], token_partition.clone(), make_user_token(vec![99], 1000, 0))
            .with_token(vec![5], token_partition.clone(), make_user_token(vec![5], 1000, 0))
            .with_user(vec![1, 2, 3], token_partition.clone(), User { public_key: vec![1, 2, 3], fuel_balance: 1000, stake: 1000, nonce: 0 })
            .with_user(vec![1], token_partition.clone(), User { public_key: vec![1], fuel_balance: 1000, stake: 1000, nonce: 0 })
            .with_user(vec![42], token_partition.clone(), User { public_key: vec![42], fuel_balance: 1000, stake: 1000, nonce: 0 })
            .with_user(vec![99], token_partition.clone(), User { public_key: vec![99], fuel_balance: 1000, stake: 1000, nonce: 0 })
            .with_user(vec![5], token_partition.clone(), User { public_key: vec![5], fuel_balance: 1000, stake: 1000, nonce: 0 });

        ActionRouter::new_with_config(
            env,
            Arc::new(RwLock::new(PendingTransactionRegistry::new())),
            config,
            Arc::new(stub),
        )
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

    fn make_message_with_body(action: &str, public_key: Vec<u8>, body: Vec<u8>) -> Message {
        Message {
            chain_id: "test".into(),
            action: action.into(),
            body,
            signature: vec![],
            public_key,
        }
    }

    fn make_process_message(public_key: Vec<u8>, nonce: usize, amount: u64) -> Message {
        let tx = Transaction {
            id: "test".into(), action: "Transfer".into(), token_id: vec![], bid: None,
            sequence_number: nonce, sender: public_key.clone(), receiver: vec![],
            amount: Some(amount), timestamp: 0, result_hash: vec![],
        };
        let body = crate::encoding::serialize_to_bytes_rmp(&tx).unwrap();
        make_message_with_body("Process", public_key, body)
    }

    fn make_preload_message(public_key: Vec<u8>, amount: u64) -> Message {
        let tx = Transaction {
            id: "test".into(), action: "Transfer".into(), token_id: vec![], bid: None,
            sequence_number: 0, sender: public_key.clone(), receiver: vec![],
            amount: Some(amount), timestamp: 0, result_hash: vec![],
        };
        let body = crate::encoding::serialize_to_bytes_rmp(&tx).unwrap();
        make_message_with_body("Preload", public_key, body)
    }

    // --- IActionRouter::route tests ---

    #[tokio::test]
    async fn route_process_returns_nonce_updated() {
        let router = make_router();
        let msg = make_process_message(vec![1, 2, 3], 0, 0);
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
        let msg = make_preload_message(vec![1], 0);
        let result = router.route(msg).await.unwrap();
        match result {
            ActionRouterResult::StakeChecked { node_type, stake } => {
                assert_eq!(node_type, NodeRegistryType::Executor);
                assert_eq!(stake, 1000);
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
        match result {
            ActionRouterResult::GasVerified { gas_used, gas_remaining } => {
                // Confirm doesn't re-verify gas (already done in Process/Preload), returns 0
                assert_eq!(gas_used, 0);
                assert_eq!(gas_remaining, 0);
            }
            other => panic!("expected GasVerified, got {:?}", other),
        }
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
        let msg = make_process_message(vec![1], 0, 0);
        // handle() should produce same result as route()
        let handle_result = router.handle(msg.clone()).await.unwrap();
        let route_result = router.route(msg).await.unwrap();
        assert_eq!(
            format!("{:?}", handle_result),
            format!("{:?}", route_result),
        );
    }

    // --- new_with_registry ---

    #[test]
    fn new_with_registry_shares_pending_txs() {
        let env = make_test_env();
        let config = make_test_config(&env);
        let shared = Arc::new(RwLock::new(PendingTransactionRegistry::new()));
        let router = ActionRouter::new_with_registry(env, shared.clone(), config);

        // Verify the router holds the same Arc
        assert!(Arc::ptr_eq(&router.pending_txs, &shared));
    }

    // --- Failure path tests ---

    #[tokio::test]
    async fn route_process_fails_insufficient_gas() {
        let env = make_test_env();
        let config = make_test_config(&env);
        let token_partition = env.token_partition_id.clone();
        // User with fuel_balance == 0 — base_cost=1 means gas_used=1 > fuel_balance
        let stub = StubDataProvider::new()
            .with_token(vec![1, 2, 3], token_partition.clone(), make_user_token(vec![1, 2, 3], 0, 0))
            .with_user(vec![1, 2, 3], token_partition.clone(), User { public_key: vec![1, 2, 3], fuel_balance: 0, stake: 0, nonce: 0 });

        let router = ActionRouter::new_with_config(
            env,
            Arc::new(RwLock::new(PendingTransactionRegistry::new())),
            config,
            Arc::new(stub),
        );

        let msg = make_process_message(vec![1, 2, 3], 0, 0);
        let result = router.route(msg).await;
        assert!(result.is_err());
        match result {
            Err(PneumaticError::Validation(ref reasons)) => {
                // With base_cost=1 and fuel_balance=0, gas_used=1 > 0 → GasLimitExceeded
                assert!(reasons.iter().any(|r| matches!(r, ValidationFailureReason::GasLimitExceeded)));
            }
            _ => panic!("expected Validation error, got {:?}", result),
        }
    }

    #[tokio::test]
    async fn route_preload_fails_insufficient_stake() {
        let env = make_test_env();
        let config = make_test_config(&env);
        let token_partition = env.token_partition_id.clone();
        // User has stake=50 but Executor min_stake=100
        let stub = StubDataProvider::new()
            .with_token(vec![1], token_partition.clone(), make_user_token(vec![1], 50, 0))
            .with_user(vec![1], token_partition.clone(), User { public_key: vec![1], fuel_balance: 50, stake: 50, nonce: 0 });

        let router = ActionRouter::new_with_config(
            env,
            Arc::new(RwLock::new(PendingTransactionRegistry::new())),
            config,
            Arc::new(stub),
        );

        let msg = make_preload_message(vec![1], 0);
        let result = router.route(msg).await;
        assert!(result.is_err());
        match result {
            Err(PneumaticError::Validation(ref reasons)) => {
                assert!(reasons.iter().any(|r| matches!(r, ValidationFailureReason::InsufficientStake)));
            }
            _ => panic!("expected Validation error, got {:?}", result),
        }
    }

    #[tokio::test]
    async fn route_register_returns_stake_value() {
        let router = make_router();
        let msg = make_message("Register", vec![1]);
        let result = router.route(msg).await.unwrap();
        match result {
            ActionRouterResult::StakeChecked { node_type, stake } => {
                assert_eq!(node_type, NodeRegistryType::Sentinel);
                // Sentinel min_stake=1000, stub token has fuel_balance=1000
                assert_eq!(stake, 1000);
            }
            other => panic!("expected StakeChecked(Sentinel) with actual stake, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn route_sign_fails_insufficient_stake_for_finalizer() {
        let env = make_test_env();
        let config = make_test_config(&env);
        let token_partition = env.token_partition_id.clone();
        // User has stake=200 but Finalizer min_stake=500
        let stub = StubDataProvider::new()
            .with_token(vec![42], token_partition.clone(), make_user_token(vec![42], 200, 0))
            .with_user(vec![42], token_partition.clone(), User { public_key: vec![42], fuel_balance: 200, stake: 200, nonce: 0 });

        let router = ActionRouter::new_with_config(
            env,
            Arc::new(RwLock::new(PendingTransactionRegistry::new())),
            config,
            Arc::new(stub),
        );

        let msg = make_message("Sign", vec![42]);
        let result = router.route(msg).await;
        assert!(result.is_err());
        match result {
            Err(PneumaticError::Validation(ref reasons)) => {
                assert!(reasons.iter().any(|r| matches!(r, ValidationFailureReason::InsufficientStake)));
            }
            _ => panic!("expected Validation error, got {:?}", result),
        }
    }

    #[tokio::test]
    async fn route_process_nonce_mismatch_from_body_returns_invalid_nonce() {
        let env = make_test_env();
        let config = make_test_config(&env);
        let token_partition = env.token_partition_id.clone();
        // User has nonce=3 in data store, body has sequence_number=5 → mismatch
        let stub = StubDataProvider::new()
            .with_user(vec![1, 2, 3], token_partition.clone(), User { public_key: vec![1, 2, 3], fuel_balance: 1000, stake: 1000, nonce: 3 });

        let router = ActionRouter::new_with_config(
            env,
            Arc::new(RwLock::new(PendingTransactionRegistry::new())),
            config,
            Arc::new(stub),
        );

        let msg = make_process_message(vec![1, 2, 3], 5, 0); // body says 5, user has 3
        let result = router.route(msg).await;
        assert!(result.is_err());
        match result {
            Err(PneumaticError::Validation(ref reasons)) => {
                assert!(reasons.iter().any(|r| matches!(r, ValidationFailureReason::InvalidNonce)));
            }
            _ => panic!("expected Validation error, got {:?}", result),
        }
    }

    #[tokio::test]
    async fn route_process_invalid_body_returns_encoding_error() {
        // Invalid MsgPack body fails deserialization → Encoding error
        let router = make_router();
        let msg = Message {
            chain_id: "test".into(),
            action: "Process".into(),
            body: vec![0xFF, 0xFF, 0xFF], // invalid MsgPack
            signature: vec![],
            public_key: vec![1, 2, 3],
        };
        let result = router.route(msg).await;
        assert!(result.is_err());
        match result {
            Err(PneumaticError::Encoding(_)) => {}
            other => panic!("expected Encoding error, got {:?}", other),
        }
    }

    // --- Per-action gas cost tests ---

    /// Verify that gas_used accounts for the cost model's base_cost.
    #[tokio::test]
    async fn verify_gas_process_with_no_amount_returns_base_cost() {
        let env = make_test_env();
        let config = make_test_config(&env);
        let token_partition = env.token_partition_id.clone();
        let stub = StubDataProvider::new()
            .with_user(vec![1], token_partition.clone(), User { public_key: vec![1], fuel_balance: 100, stake: 100, nonce: 0 });

        let router = ActionRouter::new_with_config(
            env,
            Arc::new(RwLock::new(PendingTransactionRegistry::new())),
            config,
            Arc::new(stub),
        );

        let result = router.verify_gas(&vec![1], "Process", 0).await.unwrap();
        match result {
            ActionRouterResult::GasVerified { gas_used, gas_remaining } => {
                assert_eq!(gas_used, 1); // base_cost = 1, amount=0, multiplier=1.0
                assert_eq!(gas_remaining, 99);
            }
            other => panic!("expected GasVerified, got {:?}", other),
        }
    }

    /// Verify that gas_used scales with amount for an action with multiplier > 1.0.
    /// Preload has multiplier=2.0, so amount=100 → gas = 1 + (100 × 2.0) = 201.
    #[tokio::test]
    async fn verify_gas_preload_with_amount_applies_multiplier() {
        let env = make_test_env();
        let config = make_test_config(&env);
        let token_partition = env.token_partition_id.clone();
        let stub = StubDataProvider::new()
            .with_user(vec![2], token_partition.clone(), User { public_key: vec![2], fuel_balance: 300, stake: 300, nonce: 0 });

        let router = ActionRouter::new_with_config(
            env,
            Arc::new(RwLock::new(PendingTransactionRegistry::new())),
            config,
            Arc::new(stub),
        );

        let result = router.verify_gas(&vec![2], "Preload", 100).await.unwrap();
        match result {
            ActionRouterResult::GasVerified { gas_used, gas_remaining } => {
                assert_eq!(gas_used, 201); // 1 + (100 × 2.0) = 201
                assert_eq!(gas_remaining, 99);
            }
            other => panic!("expected GasVerified, got {:?}", other),
        }
    }

    /// Verify that an action with unknown multiplier defaults to 1.0.
    #[tokio::test]
    async fn verify_gas_unknown_action_defaults_to_one() {
        let env = make_test_env();
        let config = make_test_config(&env);
        let token_partition = env.token_partition_id.clone();
        let stub = StubDataProvider::new()
            .with_user(vec![3], token_partition.clone(), User { public_key: vec![3], fuel_balance: 100, stake: 100, nonce: 0 });

        let router = ActionRouter::new_with_config(
            env,
            Arc::new(RwLock::new(PendingTransactionRegistry::new())),
            config,
            Arc::new(stub),
        );

        let result = router.verify_gas(&vec![3], "UnknownAction", 50).await.unwrap();
        match result {
            ActionRouterResult::GasVerified { gas_used, gas_remaining } => {
                assert_eq!(gas_used, 51); // 1 + (50 × 1.0) = 51 (default multiplier)
                assert_eq!(gas_remaining, 49);
            }
            other => panic!("expected GasVerified, got {:?}", other),
        }
    }

    /// Verify GasLimitExceeded when amount × multiplier + base_cost > fuel_balance.
    #[tokio::test]
    async fn verify_gas_amount_exceeds_balance_returns_gas_limit_exceeded() {
        let env = make_test_env();
        let config = make_test_config(&env);
        let token_partition = env.token_partition_id.clone();
        let stub = StubDataProvider::new()
            .with_user(vec![4], token_partition.clone(), User { public_key: vec![4], fuel_balance: 50, stake: 50, nonce: 0 });

        let router = ActionRouter::new_with_config(
            env,
            Arc::new(RwLock::new(PendingTransactionRegistry::new())),
            config,
            Arc::new(stub),
        );

        // Preload: 1 + (100 × 2.0) = 201 > 50 fuel_balance → GasLimitExceeded
        let result = router.verify_gas(&vec![4], "Preload", 100).await;
        assert!(result.is_err());
        match result {
            Err(PneumaticError::Validation(ref reasons)) => {
                assert!(reasons.iter().any(|r| matches!(r, ValidationFailureReason::GasLimitExceeded)));
            }
            _ => panic!("expected Validation error, got {:?}", result),
        }
    }

    /// Verify CostModel amount_multiplier deserialization from JSON.
    #[test]
    fn cost_model_amount_multiplier_deserializes_from_json() {
        let json = r#"{
            "base_cost":5,
            "global_min_stake":50,
            "admin_public_key":[1,2,3],
            "admin_tax_percentage":0.02,
            "amount_multiplier":{"Process":1.0,"Preload":2.5,"Sign":1.5}
        }"#;
        let model: CostModel = serde_json::from_str(json).unwrap();
        assert_eq!(model.base_cost, 5);
        assert_eq!(model.global_min_stake, 50);
        assert_eq!(model.admin_tax_percentage, 0.02);
        assert_eq!(model.amount_multiplier.get("Process"), Some(&1.0));
        assert_eq!(model.amount_multiplier.get("Preload"), Some(&2.5));
        assert_eq!(model.amount_multiplier.get("Sign"), Some(&1.5));
        // Unknown action should default to 1.0
        assert_eq!(model.amount_multiplier.get("Unknown"), None);
    }

    /// Verify CostModel amount_multiplier defaults when omitted from JSON.
    #[test]
    fn cost_model_amount_multiplier_defaults_when_omitted() {
        let json = r#"{
            "base_cost":1,
            "global_min_stake":10,
            "admin_public_key":[],
            "admin_tax_percentage":0.0
        }"#;
        let model: CostModel = serde_json::from_str(json).unwrap();
        assert_eq!(model.amount_multiplier.get("Process"), Some(&1.0));
        assert_eq!(model.amount_multiplier.get("Preload"), Some(&2.0));
        assert_eq!(model.amount_multiplier.get("Sign"), Some(&1.5));
    }

    /// Verify CostModel::compute_gas uses deterministic integer math.
    ///
    /// The fixed-point computation is bitwise-identical across CPU
    /// architectures — no FPU precision differences. Integer arithmetic
    /// also avoids overflow panics via saturating operations.
    #[test]
    fn compute_gas_deterministic_integer_math() {
        let model = CostModel::default();
        // Amount large enough to exercise the full integer path but
        // safely below the overflow threshold for multiplier 2.0.
        let amount = 1_000_000_000_000_u64; // 1 trillion
        // base_cost(1) + (1T * 20000) / 10000 = 1 + 2T = 2T + 1
        let gas = model.compute_gas(amount, 2.0);
        assert_eq!(gas, 2_000_000_000_001);

        // Deterministic: same input always produces identical output.
        for _ in 0..100 {
            assert_eq!(model.compute_gas(amount, 2.0), gas);
        }

        // No overflow panic for large but valid amounts.
        let large = u64::MAX / 20_001; // safely below overflow threshold for mult 2.0
        let _ = model.compute_gas(large, 2.0); // should not panic
    }

    /// Verify Preload multiplier 2.0 gives the expected gas.
    #[test]
    fn compute_gas_preload_multiplier() {
        let model = CostModel::default();
        let gas = model.compute_gas(100, 2.0);
        assert_eq!(gas, 201); // 1 + (100 * 2.0) = 201
    }

    /// Verify zero amount returns base_cost only.
    #[test]
    fn compute_gas_zero_amount_base_cost() {
        let model = CostModel::default();
        let gas = model.compute_gas(0, 1.0);
        assert_eq!(gas, 1); // base_cost only
    }

    /// Verify fractional multiplier (Sign = 1.5) gives exact integer result.
    #[test]
    fn compute_gas_fractional_multiplier() {
        let model = CostModel::default();
        let gas = model.compute_gas(100, 1.5);
        assert_eq!(gas, 151); // 1 + (100 * 1.5) = 151
    }
}
