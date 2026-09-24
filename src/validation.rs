use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use ff::PrimeField;
use halo2_proofs::pasta::Fp;
use serde::Serialize;
use crate::data::DataProvider;
use crate::environment::EnvironmentMetadata;
use crate::errors::{ValidationFailureReason, TransactionRiskFactor, PneumaticError, ReconciledSignatures};
use crate::shielded::{
    action_circuit_for_verifying_key, PublicInputs, ShieldedVerifier, public_inputs_from_shielded_tx,
};
use crate::tokens::Token;
use crate::transactions::{ShieldedTransaction, Transaction, TransactionValidationResult};
// The pre-split paths `pneumatic_core::validation::*` survive unchanged:
// the children own the definitions; the parent re-exports them.
pub use self::traits::{
    BlockValidationResult, BlockValidatorSpec, MerkleRootHistory, NullifierMembership,
    TransactionValidationSpec,
};
pub use self::self_signed::SelfSignedBlockValidatorSpec;
pub use self::executed::ExecutedBlockValidatorSpec;
pub use self::registries::{BlockValidatorSpecRegistry, ValidationSpecRegistry};
pub use self::shielded::{RootSnapshot, ShieldedValidationDeps, ShieldedValidationSpec};
// pub(crate) items the test suite reaches through the parent namespace:
#[cfg(test)]
pub(crate) use self::shielded::SHIELDED_VALIDATOR_VERIFIER;

pub mod executed;
pub mod registries;
pub mod self_signed;
pub mod shielded;
pub mod traits;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    pub mod helpers;
    mod executed;
    mod registries;
    mod self_signed;
    mod shielded;
}
