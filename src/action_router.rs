use crate::environment::EnvironmentMetadata;
use crate::errors::PneumaticError;
use crate::messages::Message;
use crate::node::NodeRegistryType;

// ---------------------------------------------------------------------------
// IActionRouter — routes messages to appropriate handlers
// ---------------------------------------------------------------------------

/// Trait for routing incoming messages to the appropriate handler
/// based on the message action type.
pub trait IActionRouter: Send + Sync {
    /// Route a message to the correct handler based on its action.
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
}

impl ActionRouter {
    pub fn new(environment: EnvironmentMetadata) -> Self {
        ActionRouter { environment }
    }
}

impl ActionRouter {
    /// Route a message to the appropriate handler based on action.
    /// Handles utility token coordination.
    pub async fn handle(&self, message: Message) -> Result<ActionRouterResult, PneumaticError> {
        match message.action.as_str() {
            "Process" | "Preload" | "Sign" => {
                // Forward to the appropriate node type handler
                // (Sentinel for Process/Preload, Finalizer for Sign)
                Ok(ActionRouterResult::UnknownAction(message.action))
            }
            "Confirm" | "Reject" => {
                // Forward to Committer handlers
                Ok(ActionRouterResult::UnknownAction(message.action))
            }
            "Register" => {
                // Handle node registration
                Ok(ActionRouterResult::StakeChecked {
                    node_type: NodeRegistryType::Sentinel,
                    stake: 0, // Placeholder
                })
            }
            "Clear" => {
                // Clear transaction state
                Ok(ActionRouterResult::NonceUpdated {
                    sender: vec![],
                    new_nonce: 0,
                })
            }
            "DistributeToken" => {
                Ok(ActionRouterResult::TokenDispatched {
                    token_id: vec![],
                })
            }
            _ => Ok(ActionRouterResult::UnknownAction(message.action)),
        }
    }
}
