//! ADR-004 enforcement at the Committer: a block is only committed when its own
//! routing decision says the node that sent it was the Finalizer chosen for it.
//!
//! This is the layer where a routing failure becomes money — a committed block is a
//! state change — so it is also the layer where "who sent it?" used to be answered by
//! the envelope alone: any registered Finalizer could deliver any finalized block, and
//! the first to arrive won. The refusals below are what replaces that.

use super::helpers::*;
use super::super::*;
use pneumatic_core::blocks::BlockFactory;
use pneumatic_core::transactions::SelectionRecord;

/// Re-hash after mutating the embedded transaction. The committer verifies
/// `current_hash` before anything else, so a mutated block that was not re-hashed
/// would fail for the wrong reason and the test would prove nothing.
fn reseal(mut block: pneumatic_core::blocks::Block) -> pneumatic_core::blocks::Block {
    block.current_hash = BlockFactory::create_hash(&block).expect("re-hash");
    block
}

fn committer_fixture() -> (Committer, Arc<PendingTransactionRegistry>, Arc<CollectingLogger>) {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, registry, logger) = make_test_committer(dp);
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);
    (committer, registry, logger)
}

/// The block's transaction carries no routing decision. On a non-shielded
/// transaction that is not "unassigned, use judgement" — the Commit path is the
/// Finalizer path, so a block with no decision is a block whose author cannot be named.
#[tokio::test]
async fn a_block_whose_transaction_was_never_routed_is_refused() {
    let (committer, _registry, _logger) = committer_fixture();
    let block = make_gossip_block(&committer, "unrouted_tx", b"proposer".to_vec());
    let tip = block.previous_hash.clone();
    let mut block = make_gossip_block_at_prev("unrouted_tx", b"proposer".to_vec(), &tip);
    block.signed_trans.transaction.selection = None;
    let block = reseal(block);

    let message = make_block_finalized_message(block);
    let err = committer
        .handle_block_finalized(message)
        .await
        .expect_err("a block with no routing decision is refused, not accepted from the first Finalizer to deliver it");
    assert!(
        matches!(err, CommitterError::Routing(_)),
        "expected a Routing refusal, got {err:?}"
    );
}

/// The decision names a Finalizer, and it is not the node that sent the block. The
/// sender is a registered Finalizer — the envelope would pass — which is precisely the
/// gap the decision closes.
#[tokio::test]
async fn a_block_sent_by_a_finalizer_the_decision_does_not_name_is_refused() {
    let (committer, _registry, _logger) = committer_fixture();
    let tip = committer
        .tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;
    let mut block = make_gossip_block_at_prev("other_finalizer_tx", b"proposer".to_vec(), &tip);
    // Re-attest honestly (by the registered Sentinel) for a different Finalizer, and
    // sign the block with that Finalizer's key so the signature check is satisfied —
    // the only thing left to refuse on is "you are not who this says".
    let stranger = pneumatic_core::crypto::Ed25519Provider::from_seed([0xEE; 32]);
    let stranger_key = stranger.public_key().expect("stranger pk");
    block.signed_trans.transaction.selection = Some(gossip_selection_named(
        "other_finalizer_tx",
        &stranger_key,
    ));
    let transaction_hash = block.signed_trans.finalizer_sig.transaction_hash.clone();
    block.signed_trans.finalizer_addr = stranger_key.clone();
    block.signed_trans.finalizer_sig.signature =
        stranger.sign_data(&transaction_hash).expect("stranger signs");
    let block = reseal(block);

    // Delivered by `gossip_finalizer`, a registered Finalizer the decision does not name.
    let message = make_block_finalized_message(block);
    let err = committer
        .handle_block_finalized(message)
        .await
        .expect_err("the sender has to be the Finalizer the decision names");
    assert!(
        matches!(err, CommitterError::Routing(_)),
        "expected a Routing refusal, got {err:?}"
    );
}

/// A correctly signed decision whose attester is not a Sentinel.
///
/// The signature is the easy part: a routing record can be produced by any keypair,
/// including the sender's own. The role lookup is what stops a Finalizer from routing
/// work to itself, and this test is the one that would pass without it.
#[tokio::test]
async fn a_decision_attested_by_a_key_that_is_not_a_sentinel_is_refused() {
    let (committer, _registry, _logger) = committer_fixture();
    let tip = committer
        .tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;
    let mut block = make_gossip_block_at_prev("impostor_tx", b"proposer".to_vec(), &tip);

    let impostor = pneumatic_core::crypto::Ed25519Provider::from_seed([0x99; 32]);
    let finalizer_key = gossip_finalizer().public_key().expect("finalizer pk");
    block.signed_trans.transaction.selection = Some(
        SelectionRecord::attest(
            &impostor,
            "impostor_tx",
            pneumatic_core::epoch::FINALIZER_DOMAIN,
            finalizer_key,
            0,
            vec![0x5A; 32],
        )
        .expect("the impostor attests"),
    );
    let block = reseal(block);

    let message = make_block_finalized_message(block);
    let err = committer
        .handle_block_finalized(message)
        .await
        .expect_err("an attester must hold the Sentinel role, not merely have signed something");
    assert!(
        matches!(err, CommitterError::Routing(_)),
        "expected a Routing refusal, got {err:?}"
    );
}

/// The block's finalizer signature is verified under the key the *decision* names —
/// not under `finalizer_addr`, which is a field inside the block itself.
///
/// Positive rather than negative on purpose: it pins the substitution. Here the two
/// keys disagree, the signature comes from the routed key, and the block is accepted.
/// Had the check stayed on `finalizer_addr`, this block would be rejected and a block
/// signed by the routed key but naming another address would be the one that slipped
/// through.
#[tokio::test]
async fn the_finalizer_signature_is_checked_against_the_routed_key_not_the_self_reported_address() {
    let (committer, _registry, _logger) = committer_fixture();
    let tip = committer
        .tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;
    let mut block = make_gossip_block_at_prev("routed_key_tx", b"proposer".to_vec(), &tip);

    // Routed to `gossip_finalizer` (the sender), and genuinely signed by it — but the
    // self-reported address inside the block claims a different key.
    let wrong_addr = pneumatic_core::crypto::Ed25519Provider::from_seed([0x77; 32]);
    block.signed_trans.finalizer_addr = wrong_addr.public_key().expect("wrong addr pk");
    let block = reseal(block);

    let message = make_block_finalized_message(block);
    committer
        .handle_block_finalized(message)
        .await
        .expect("the routed key signed this block, so it is accepted even though \
                 finalizer_addr claims otherwise — the signature is verified under the \
                 key the decision names");

    let chain_len = committer
        .tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_count();
    assert!(
        chain_len >= 2,
        "the block has to have actually been appended, got a chain of {chain_len}"
    );
}

/// The check the four-host rehearsal found missing.
///
/// A routing record travels inside the block it authorizes, so on its own it is a
/// claim the sender writes about itself. Checks 1-3 of `block_authority` validate that
/// claim (attestation, attester role, sender naming) and every block in the rehearsal
/// passed them — each host's own Sentinel had routed the transaction to that host's own
/// Finalizer, and each host then delivered a block that named its own sender. What makes
/// the decision a constraint on *this* committer is that it compares the incoming record
/// with the record its own Sentinel recorded.
#[tokio::test]
async fn a_block_carrying_a_different_decision_for_the_same_transaction_is_refused() {
    let (committer, registry, _logger) = committer_fixture();
    let tip = committer
        .tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;

    // What this node holds: the decision its own Sentinel wrote when it admitted the
    // transaction. Stamped directly, because the fixture has no Sentinel in-process.
    let tx = Transaction {
        selection: Some(gossip_selection("contested_tx")),
        ..make_gossip_block_at_prev("contested_tx", b"proposer".to_vec(), &tip)
            .signed_trans
            .transaction
    };
    registry
        .add_transaction(
            "contested_tx".to_string(),
            PendingTransaction::new(
                "contested_tx".to_string(),
                TransactionState::Finalizing {
                    transaction: tx.clone(),
                    finalizer_key: gossip_finalizer().public_key().expect("finalizer pk"),
                },
            ),
        )
        .expect("the fixture transaction is added");

    // The block matching that decision is accepted — the refusal below has to be about
    // the decision, not about some unrelated defect in the fixture.
    let first = make_gossip_block_at_prev("contested_tx", b"proposer".to_vec(), &tip);
    let first_at = first.current_hash.clone();
    committer
        .handle_block_finalized(make_block_finalized_message(first))
        .await
        .expect("the block whose decision matches this node's is committed");

    // A second block for the SAME transaction, honestly attested by the registered
    // Sentinel, honestly signed, and delivered by the Finalizer it names — but naming a
    // different Finalizer than this node was told. That is the second host in the
    // rehearsal, and appending it is the divergence ADR-004 exists to prevent.
    let stranger = pneumatic_core::crypto::Ed25519Provider::from_seed([0xEE; 32]);
    let stranger_key = stranger.public_key().expect("stranger pk");
    let mut second = make_gossip_block_at_prev("contested_tx", b"proposer".to_vec(), &tip);
    second.signed_trans.transaction.selection =
        Some(gossip_selection_named("contested_tx", &stranger_key));
    second.signed_trans.finalizer_addr = stranger_key.clone();
    second.signed_trans.finalizer_sig.signature = stranger
        .sign_data(&second.signed_trans.finalizer_sig.transaction_hash)
        .expect("stranger signs");
    second.current_hash = BlockFactory::create_hash(&second).expect("re-hash");

    let err = committer
        .handle_block_finalized(make_block_finalized_message_from(second, stranger_key))
        .await
        .expect_err("a second, differently routed block for one transaction is not a second block");
    let message = match &err {
        CommitterError::Routing(m) => m.clone(),
        other => panic!("expected a Routing refusal, got {other:?}"),
    };
    assert!(
        message.contains("not the one this node holds"),
        "the refusal has to name the disagreement, got: {message}"
    );

    let chain_len = committer
        .tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_count();
    assert_eq!(
        chain_len, 2,
        "genesis plus the ONE accepted block — the refused block must not have appended"
    );
    let tip_after = committer
        .tokens
        .get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_current_chain_state()
        .last_hash_in;
    assert_eq!(
        tip_after, first_at,
        "the chain tip must still be the block this node's own decision named"
    );
}
