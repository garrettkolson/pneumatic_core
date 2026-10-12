//! ADR-015, Phase 6: the committer's idempotent `CreateTokenDelta` apply path.
//!
//! The executor emits a canonical `CreateTokenDelta` in `result_data` and the
//! finalizer signs its hash. The committer does not receive the raw bytes, but
//! the delta is a pure function of the transaction (`sender`, `nonce`,
//! `DeployParams`, `hash`) — so `apply_deploy_delta` re-derives it, verifies it
//! against the signed `result_hash`, and creates the token idempotently: the
//! token is created once, and a replayed deploy is a no-op.

use super::helpers::*;
use pneumatic_core::contracts::{deploy_contract, DeployParams};
use pneumatic_core::tokens::Token;
use pneumatic_core::crypto::HashProvider;
use std::collections::HashMap;
use std::sync::Arc;

/// QD4: the deploy apply writes the token under `partition_id ==
/// environment_id`. The committer test environment id is `"test"`.
const PARTITION: &str = "test";

/// Build a `DeployContract` transaction whose `result_hash` is the canonical
/// hash of the delta the committer will re-derive (so the integrity check in
/// `apply_deploy_delta` passes).
fn make_deploy_tx(sender: Vec<u8>, nonce: usize, params: DeployParams) -> Transaction {
    let hash = BasicHashProvider::new();
    let delta = deploy_contract(&sender, nonce as u64, &params, &hash).unwrap();
    let result_bytes = serialize_to_bytes_rmp(&delta).unwrap();
    let result_hash = hash.hash(&result_bytes);
    Transaction {
        payload: serialize_to_bytes_rmp(&params).unwrap(),
        gas_limit: 0,
        id: "deploy_tx".to_string(),
        action: "DeployContract".to_string(),
        token_id: vec![],
        bid: None,
        sequence_number: nonce,
        sender,
        receiver: vec![],
        amount: None,
        timestamp: 0,
        result_hash,
        sender_signature: vec![],
    
    result_data: vec![],selection: None,
    }
}

fn spec_params() -> DeployParams {
    DeployParams {
        name: "mycontract".to_string(),
        engine: "Spec".to_string(),
        bytecode: vec![1, 2, 3, 4, 5],
        metadata: HashMap::new(),
        owners: vec![],
        threshold: 0,
    }
}

#[test]
fn deploy_delta_creates_token_in_env_partition() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());

    let params = spec_params();
    let sender = vec![0x42; 32];
    let tx = make_deploy_tx(sender.clone(), 0, params.clone());

    committer.apply_deploy_delta(&tx).expect("deploy should apply");

    // Re-derive the expected token id to assert the token landed in the
    // environment partition with the contract metadata.
    let hash = BasicHashProvider::new();
    let delta = deploy_contract(&sender, 0, &params, &hash).unwrap();
    let token = dp.get_token(&delta.token_id, PARTITION).expect("token should exist");
    assert_eq!(token.id, delta.token_id);
    assert_eq!(token.environment_id, "test");
    assert_eq!(
        token.metadata.get("token_type").map(|s| s.as_str()),
        Some("contract")
    );
    assert_eq!(
        token.metadata.get("contract_engine").map(|s| s.as_str()),
        Some("Spec")
    );
}

#[test]
fn deploy_delta_is_idempotent_on_replay() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());

    let params = spec_params();
    let sender = vec![0x42; 32];
    let tx = make_deploy_tx(sender.clone(), 0, params.clone());

    committer.apply_deploy_delta(&tx).expect("first apply");
    // A replayed deploy must be a no-op, not an error.
    committer.apply_deploy_delta(&tx).expect("replay is a no-op");

    let hash = BasicHashProvider::new();
    let delta = deploy_contract(&sender, 0, &params, &hash).unwrap();
    let token = dp.get_token(&delta.token_id, PARTITION).expect("token should exist");
    assert_eq!(token.id, delta.token_id);
}

#[test]
fn deploy_delta_rejects_tampered_result_hash() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp.clone());

    let params = spec_params();
    let sender = vec![0x42; 32];
    let mut tx = make_deploy_tx(sender.clone(), 0, params.clone());
    // Tamper with the result hash — the integrity check must fail closed.
    tx.result_hash = vec![0xFF; 32];

    let err = committer
        .apply_deploy_delta(&tx)
        .expect_err("tampered hash must be rejected");
    assert!(matches!(
        err,
        CommitterError::TransactionPayloadMismatch(_)
    ));
}

/// Stage 2 of the convergence work: a commit applies **the block's** execution result, not
/// the one this node computed — and the delta apply is where that becomes observable.
///
/// The commit path used to refuse a block whose execution result differed from the result in
/// its own registry entry. Each host executes against its own token state, so once exactly
/// one Finalizer produces each transaction, that compare refuses every block on every host
/// except the producer's (`fact-h12-assumes-lockstep-execution`). It was narrowed to the
/// transaction's identity — everything the sender authorized, plus the routing record — and
/// the result is adopted from the block.
///
/// The observable is not a field someone remembered to copy. `apply_deploy_delta` re-derives
/// the delta and checks it against `transaction.result_hash`, so it reads whichever result
/// the commit path is holding. This fixture makes the two disagree on purpose: the local
/// entry carries a result this node "computed" that does not match the deploy it contains,
/// and the block carries the correct one. So the test fails two ways — if the payload
/// compare is still full-hash, the commit is refused outright; and if the compare is
/// narrowed but the result is not adopted, the apply reads the stale local value and refuses
/// with `TransactionPayloadMismatch`. A token in the environment partition is the proof that
/// neither happened: the state this node applied is the state in the block.
#[tokio::test]
async fn the_commit_applies_the_blocks_result_rather_than_the_local_one() {
    let dp = Arc::new(TestDataProvider::new());
    dp.insert_user(
        b"alice".to_vec(),
        "token".to_string(),
        User {
            public_key: b"alice".to_vec(),
            fuel_balance: 1000,
            stake: 0,
            nonce: 0,
        },
    );
    let (committer, registry, _logger) = make_test_committer(dp.clone());
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let tx_id = "deploy_foreign_result";
    let mut block = make_test_block_for_token(&committer, tx_id, b"alice".to_vec());
    let sender = block.signed_trans.transaction.sender.clone();
    let nonce = block.signed_trans.transaction.sequence_number;
    let params = spec_params();
    let hash = BasicHashProvider::new();
    let delta = deploy_contract(&sender, nonce as u64, &params, &hash).expect("delta derives");
    let producer_result = hash
        .hash(&serialize_to_bytes_rmp(&delta).expect("delta serializes"));

    // The block as the routed Finalizer produced it: a deploy, with the result that deploy
    // actually yields. Re-signed after the edit, because the result is inside the finalizer's
    // signed digest — an unsigned edit is tampering, and would be refused for that reason.
    block.signed_trans.transaction.action = "DeployContract".into();
    block.signed_trans.transaction.payload =
        serialize_to_bytes_rmp(&params).expect("params serialize");
    block.signed_trans.transaction.result_hash = producer_result.clone();
    sign_like_finalizer(&mut block.signed_trans, &gossip_finalizer());
    block.current_hash = pneumatic_core::blocks::BlockFactory::create_hash(&block)
        .expect("the edited block re-hashes");

    // This node's own record of the same transaction: identical in every field the sender
    // authorized, and carrying a result this node arrived at somewhere else. In the rehearsal
    // this was not a contrivance — it is what a host that had committed a different set of
    // blocks before this one produced.
    let mut local = block.signed_trans.transaction.clone();
    local.result_hash = vec![0x11; 32];
    local.result_data = b"what-this-node-computed-instead".to_vec();
    make_finalizing_entry(&registry, tx_id, b"alice".to_vec());
    {
        let mut entry = registry
            .get_transaction_mut(tx_id)
            .expect("the fixture entry exists");
        match &mut entry.state {
            pneumatic_core::transactions::TransactionState::Finalizing { transaction, .. } => {
                *transaction = local;
            }
            other => panic!("the fixture entry must be Finalizing, got {other:?}"),
        }
    }

    let commit = TransactionCommit {
        trans_id: tx_id.as_bytes().to_vec(),
        token_id: vec![1],
        env_id: "test".to_string(),
        proposed_block: block,
    };
    committer
        .check_and_commit_transaction_results(&commit, vec![])
        .await
        .expect("the block's result is the routed Finalizer's answer; this node's differing \
                 local result is not a reason to refuse the block");

    let applied = dp
        .get_token(&delta.token_id, PARTITION)
        .expect("the contract token the BLOCK described must have been created — the apply \
                 read the block's result, not the stale local one");
    assert_eq!(applied.id, delta.token_id);
    assert_eq!(
        applied.metadata.get("contract_engine").map(|s| s.as_str()),
        Some("Spec"),
        "and it is the contract the block's payload describes"
    );
}
