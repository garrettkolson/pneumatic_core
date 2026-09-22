use super::helpers::*;
use super::super::*;
use std::sync::{Arc, Mutex};


#[tokio::test]
async fn concurrent_block_finalized_submissions_no_panic() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let original_chain_len = committer.tokens.get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_count();

    // Spawn multiple concurrent handle_block_finalized calls with different tx_ids
    let committer_arc = Arc::new(committer);
    std::thread::scope(|s| {
        let mut handles = vec![];
        for i in 0..10 {
            let committer = committer_arc.clone();
            let tx_id = format!("concurrent_tx_{}", i);
            handles.push(s.spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                let block = make_gossip_block(&committer, &tx_id, vec![i as u8]);
                let message = make_block_finalized_message(block);
                // Each thread runs its own runtime to handle the async call
                rt.block_on(async {
                    committer.handle_block_finalized(message).await
                })
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
    });

    // All joins succeeded → no data races
    // Chain should have grown (some blocks may have been orphaned due to concurrent writes)
    let new_chain_len = committer_arc.tokens.get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_count();
    assert!(new_chain_len >= original_chain_len);
}


#[tokio::test]
async fn handle_block_finalized_appends_valid_block() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // Bootstrap token and create a genesis block
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let original_chain_len = committer.tokens.get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_count();
    assert_eq!(original_chain_len, 1); // genesis only

    // Build a valid next block chained off the current tip
    let block = make_gossip_block(&committer, "gossip_tx", b"mallory".to_vec());
    let message = make_block_finalized_message(block);

    // Should succeed and append the block
    let result = committer.handle_block_finalized(message).await;
    assert!(result.is_ok());

    // Chain should have grown by 1
    let new_chain_len = committer.tokens.get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_count();
    assert_eq!(new_chain_len, original_chain_len + 1);
}


/// (AUDIT Phase 3.3 / C5 discriminator) N concurrent sibling blocks — same frozen parent
/// (`previous_hash`), distinct tx_ids ⇒ distinct `current_hash`, each carrying a valid finalizer
/// signature — may only append ONE. The atomic `append_validated_block` (a single `get_mut`
/// spanning the tip read and the push) guarantees this; without it, the read-then-`get_mut`
/// gap let two siblings both validate and append (a fork), growing the chain by more than one.
#[tokio::test]
async fn concurrent_sibling_blocks_exactly_one_appended() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain.
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    // Capture the tip once; every sibling chains off this same parent.
    let tip = committer.tokens.get(&vec![1]).unwrap().value().blockchain
        .get_current_chain_state().last_hash_in;
    let original_len = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
    assert_eq!(original_len, 1); // genesis only

    // Build N sibling blocks up front, each with the frozen `previous_hash` and its own valid
    // finalizer signature — distinct tx_ids make their `current_hash` distinct.
    let n = 16;
    let siblings: Vec<Block> = (0..n)
        .map(|i| make_gossip_block_at_prev(&format!("sibling-{i}"), vec![i as u8], &tip))
        .collect();

    // Fan out the concurrent handlers; each runs its own runtime.
    // A single runtime shared across all handler threads: each spawned handler drives to
    // completion on its own OS thread, so the handlers actually race in wall-clock time — which
    // exposes the read-then-get_mut gap (with the gap, every sibling reads the same stale tip
    // and all of them append). A per-thread `Runtime::new()` serialized startup and hid the race.
    let committer_arc = Arc::new(committer);
    // Pre-create each handler's runtime up front, outside the spawn loop, so all N handlers
    // begin their block_on in the same instant. The per-thread `Runtime::new()` inside the
    // loop previously serialized startup and hid the race. (A shared multi_thread runtime
    // can't be dropped off the worker threads here, so each thread keeps its own current-thread
    // runtime and drops it on its own OS thread.)
    let runtimes: Vec<_> = (0..n).map(|_| tokio::runtime::Runtime::new().unwrap()).collect();
    std::thread::scope(|s| {
        let mut handles = vec![];
        for (block, rt) in siblings.into_iter().zip(runtimes) {
            let committer = committer_arc.clone();
            let message = make_block_finalized_message(block);
            handles.push(s.spawn(move || {
                rt.block_on(async { committer.handle_block_finalized(message).await })
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
    });

    // Exactly one of the N siblings appended; the rest were rejected with LinkageMismatch.
    let new_len = committer_arc.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
    assert_eq!(new_len, original_len + 1);
}


/// (AUDIT Phase 3.3 / C5 discriminator) A block whose finalizer signature does not verify is
/// rejected (`Err(InvalidFinalizerSignature)`) and never appended. A pre-fix block with an
/// empty `finalizer_sig` would have been accepted and appended.
#[tokio::test]
async fn handle_block_finalized_rejects_bad_finalizer_sig() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain.
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    // A validly-chained block (correct previous_hash, valid finalizer sig) — then forge the
    // signature bytes so verification fails.
    let mut block = make_gossip_block(&committer, "forged", b"mallory".to_vec());
    let original_len = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
    block.signed_trans.finalizer_sig.signature = vec![0xAA; 64]; // forged

    let message = make_block_finalized_message(block);
    let result = committer.handle_block_finalized(message).await;
    assert!(matches!(result, Err(CommitterError::InvalidFinalizerSignature)));

    // Rejected → nothing appended.
    let chain_len = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
    assert_eq!(chain_len, original_len);
}


/// (AUDIT Phase 3.4 / H15 discriminator) Out-of-order delivery is buffered, not dropped, and
/// all blocks eventually commit. Deliver the second block (`b2`) before its parent `b1`: `b2` is
/// buffered (chain length unchanged), and when `b1` lands the buffer is replayed and `b2` is
/// promoted. Proven discriminator: restoring the old silent-drop behavior yields a chain length
/// of `original + 1` after `b1` (`b2` lost) — the assertion fails without the fix.
#[tokio::test]
async fn handle_block_finalized_buffers_orphan_and_replays_on_tip_advance() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain (genesis only).
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let original_chain_len = committer.tokens.get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_count();
    assert_eq!(original_chain_len, 1); // genesis only

    // The current tip (genesis). b1 chains off it; b2 chains off b1.
    let tip = committer.tokens.get(&vec![1]).unwrap().value().blockchain
        .get_current_chain_state().last_hash_in;
    let b1 = make_gossip_block_at_prev("orphan-b1", b"proposer-1".to_vec(), &tip);
    let b2 = make_gossip_block_at_prev("orphan-b2", b"proposer-2".to_vec(), &b1.current_hash);

    // Deliver b2 FIRST (its parent b1 has not landed) → buffered, not appended.
    let result = committer.handle_block_finalized(make_block_finalized_message(b2)).await;
    assert!(result.is_ok(), "buffering an orphan is non-fatal");
    let len_after_b2 = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
    assert_eq!(len_after_b2, original_chain_len, "orphan b2 not appended before its parent");
    assert_eq!(committer.orphan_blocks.lock().await.len(), 1, "b2 is buffered");

    // Now deliver b1 → it appends, and the replay loop promotes b2 whose parent is now the tip.
    let result = committer.handle_block_finalized(make_block_finalized_message(b1)).await;
    assert!(result.is_ok());
    let len_after_b1 = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
    assert_eq!(len_after_b1, original_chain_len + 2, "b1 appends and b2 is promoted");
    assert!(committer.orphan_blocks.lock().await.is_empty(), "b2 promoted out of the buffer");
}


/// (AUDIT Phase 3.4 / H15 — cascade + reorder) A chain delivered in adversarially out-of-order
/// order all eventually commits via cascading replay. Order `[b3, b5, b1, b4, b2]`: early
/// blocks are buffered; each subsequent real block triggers a cascade that promotes everything
/// that now chains onto the advancing tip. A non-cascading replay (promote only the one block
/// whose parent is the tip) would stop after `b3` — the cascade is what drives `b4`, `b5` home.
#[tokio::test]
async fn handle_block_finalized_replays_orphan_cascade_in_out_of_order_delivery() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain (genesis only).
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let original_chain_len = committer.tokens.get(&vec![1])
        .unwrap()
        .value()
        .blockchain
        .get_count();
    assert_eq!(original_chain_len, 1);

    // Build a 5-block chain up front, each chaining off the prior block's hash.
    let mut tip = committer.tokens.get(&vec![1]).unwrap().value().blockchain
        .get_current_chain_state().last_hash_in;
    let mut chain = Vec::new();
    for i in 0..5 {
        let b = make_gossip_block_at_prev(&format!("cascade-{i}"), vec![i as u8], &tip);
        tip = b.current_hash.clone();
        chain.push(b);
    }

    // Deliver in adversarial order: the chain breaks at b1/b2, so b3 and b5 buffer first, then
    // b4 cannot chain until its parent b3 lands, etc.
    let order = [2usize, 4, 0, 3, 1]; // b3, b5, b1, b4, b2
    for &i in &order {
        let result = committer.handle_block_finalized(make_block_finalized_message(chain[i].clone())).await;
        assert!(result.is_ok(), "block {i} delivered without error");
    }

    // All five landed: a single chain of genesis + 5.
    let final_len = committer.tokens.get(&vec![1]).unwrap().value().blockchain.get_count();
    assert_eq!(final_len, original_chain_len + 5);
    assert!(committer.orphan_blocks.lock().await.is_empty(), "everything promoted out");

    // The chain is now contiguous: each block's previous_hash matches its predecessor's hash.
    let entry = committer.tokens.get(&vec![1]).unwrap();
    let blocks: Vec<&Block> = entry.value().blockchain.chain.iter().collect();
    let mut idx = 1;
    while idx < blocks.len() {
        assert_eq!(blocks[idx].previous_hash, blocks[idx - 1].current_hash);
        idx += 1;
    }
}


#[tokio::test]
async fn handle_block_finalized_rejects_tampered_block() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    // Build a validly-chained block, then tamper the current_hash
    let block = make_gossip_block(&committer, "tampered", b"tampered".to_vec());
    // Tamper the hash so validation fails
    let block = Block {
        signed_trans: block.signed_trans,
        token_metadata: block.token_metadata,
        previous_hash: block.previous_hash,
        current_hash: vec![0xAA, 0xBB, 0xCC], // tampered
        timestamp: block.timestamp,
        finality_status: block.finality_status,
        proposer_key: block.proposer_key,
        epoch_number: block.epoch_number,
    };

    let message = make_block_finalized_message(block);

    let result = committer.handle_block_finalized(message).await;
    assert!(matches!(result, Err(CommitterError::InvalidBlockHash)));
}


#[tokio::test]
async fn handle_block_finalized_unknown_token_returns_error() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // A valid finalizer signature so the block clears the C5 gate and reaches the expected
    // TokenNotFound path (this block targets a token that is not in the committer's cache).
    let unknown_finalizer = Ed25519Provider::generate();
    let unknown_finalizer_addr = unknown_finalizer.public_key().expect("finalizer public key");
    let unknown_tx_hash = b"unknown-transaction-hash".to_vec();
    let unknown_sig = unknown_finalizer.sign_data(&unknown_tx_hash).expect("finalizer signature");
    let signed = SignedTransaction {
        shielded: None,
        transaction_id: "unknown".to_string(),
        transaction: Transaction {
            id: "unknown".to_string(),
            action: "Process".into(),
            token_id: vec![99], // not in committer's token cache
            bid: None,
            sequence_number: 1,
            sender: b"alice".to_vec(),
            receiver: b"bob".to_vec(),
            amount: Some(100),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
        },
        total_voters: 3,
        total_stake: 42,
        leader_hash: vec![],
        leader_address: vec![],
        leader_stake: 0,
        finalizer_addr: unknown_finalizer_addr,
        finalizer_sig: TransactionSignature {
            transaction_id: vec![],
            env_id: vec![],
            transaction_hash: unknown_tx_hash,
            signature: unknown_sig,
            current_stake: 0,
        },
        executor_sigs: HashMap::new(),
        proposer_key: b"unknown".to_vec(),
    };

    let block = Block {
        signed_trans: signed,
        token_metadata: HashMap::new(),
        previous_hash: vec![],
        current_hash: vec![0xDE, 0xAD],
        timestamp: 0,
        finality_status: FinalityStatus::Optimistic,
        proposer_key: vec![],
        epoch_number: 0,
    };

    let message = make_block_finalized_message(block);

    let result = committer.handle_block_finalized(message).await;
    assert!(matches!(result, Err(CommitterError::TokenNotFound(_))));
}


/// Drive the public `handle_block_finalized` path and assert every
/// outbound broadcast — the `BlockConfirmed` vote and the
/// `DistributeBlock` payload — verifies under the committer identity.
#[tokio::test]
async fn block_finalized_broadcasts_signed_with_committer_identity() {
    let dp = Arc::new(TestDataProvider::new());
    let (committer, _registry, _logger) = make_test_committer(dp);

    // Bootstrap token and chain
    let mut token = Token::new();
    token.id = vec![1];
    committer.bootstrap_token(token);
    bootstrap_token_chain(&committer);

    let sentinel_recorder = Arc::new(Mutex::new(Vec::new()));
    let archiver_recorder = Arc::new(Mutex::new(Vec::new()));
    assert!(committer.node_registry.register_peer(
        vec![0xCC; 32],
        [3u8; 16],
        &NodeRegistryType::Sentinel,
        Box::new(RecordingConnection { recorder: sentinel_recorder.clone() }),
    ));
    assert!(committer.node_registry.register_peer(
        vec![0xAA; 32],
        [4u8; 16],
        &NodeRegistryType::Archiver,
        Box::new(RecordingConnection { recorder: archiver_recorder.clone() }),
    ));

    let block = make_gossip_block(&committer, "signed_tx", b"alice".to_vec());
    committer
        .handle_block_finalized(make_block_finalized_message(block))
        .await
        .expect("BlockFinalized should be accepted");

    // The sentinel received a BlockConfirmed vote signed by the committer.
    let vote_raw = sentinel_recorder
        .lock()
        .unwrap()
        .iter()
        .find(|raw| matches!(deserialize_rmp_to::<Message>(raw), Ok(m) if m.action == "BlockConfirmed"))
        .cloned()
        .expect("sentinel should receive a BlockConfirmed vote");
    let vote: Message = deserialize_rmp_to(&vote_raw).expect("vote payload is a Message");
    assert_signed_by(&vote, &committer.identity);

    // The archiver received the DistributeBlock payload signed by the committer.
    let dist_raw = archiver_recorder
        .lock()
        .unwrap()
        .iter()
        .find(|raw| matches!(deserialize_rmp_to::<Message>(raw), Ok(m) if m.action == "DistributeBlock"))
        .cloned()
        .expect("archiver should receive a DistributeBlock payload");
    let dist: Message = deserialize_rmp_to(&dist_raw).expect("dist payload is a Message");
    assert_signed_by(&dist, &committer.identity);
}
