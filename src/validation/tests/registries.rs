//! Registry tests: register/lookup/defaults for both spec registries and
//! the `BlockValidationResult` variants.
use super::super::*;

// --- ValidationSpecRegistry ---

#[test]
fn registry_registers_and_looks_up_defaults() {
    let mut registry = ValidationSpecRegistry::new();
    registry.register_defaults();
    assert!(registry.get("SelfSigned").is_some());
    assert!(registry.get("Executed").is_some());
}

#[test]
fn registry_get_nonexistent_returns_none() {
    let registry = ValidationSpecRegistry::new();
    assert!(registry.get("Unknown").is_none());
}

// --- BlockValidationResult ---

#[test]
fn block_validation_result_variants() {
    let valid = BlockValidationResult::Valid;
    let debug_str = format!("{:?}", valid);
    assert!(debug_str.contains("Valid"));

    let invalid = BlockValidationResult::Invalid(vec![ValidationFailureReason::InvalidAmount]);
    let debug_str = format!("{:?}", invalid);
    assert!(debug_str.contains("Invalid"));
}

// --- BlockValidatorSpecRegistry ---

#[test]
fn block_validator_registry_registers_and_looks_up_defaults() {
    let mut registry = BlockValidatorSpecRegistry::new();
    registry.register_defaults();
    assert!(registry.get("SelfSigned").is_some());
    assert!(registry.get("Executed").is_some());
}

#[test]
fn block_validator_registry_get_nonexistent_returns_none() {
    let registry = BlockValidatorSpecRegistry::new();
    assert!(registry.get("Unknown").is_none());
}
