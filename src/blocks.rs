use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::Hash;
use std::vec;
use chrono::{Utc, prelude::*};
use serde::{Deserialize, Serialize};
use crate::crypto::{BasicHashProvider, HashProvider};
use crate::tokens::Token;
use crate::transactions::SignedTransaction;

/// Finality status of a block — tracks whether a block could still be superseded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinalityStatus {
    /// Optimistic: block was committed without conflict but could theoretically be
    /// overridden if a higher-stake competing proposal is later detected.
    Optimistic,
    /// Confirmed: block has been observed with no conflict for enough time to be
    /// considered irreversible.
    Confirmed,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Block {
    pub signed_trans: SignedTransaction,
    pub token_metadata: HashMap<String, String>,
    pub previous_hash: Vec<u8>,
    pub current_hash: Vec<u8>,
    pub timestamp: i64,
    /// Whether this block is optimistically committed or confirmed.
    pub finality_status: FinalityStatus,
    /// Public key of the proposer who created this block.
    ///
    /// Single semantics (AUDIT Phase 2.3, finding C2): this is the leader/proposer identity
    /// recorded on the signed transaction, i.e. always read from
    /// `SignedTransaction.proposer_key`. All block constructors (`from_transaction`,
    /// `Token::create_block`, `BlockBuilder::create_block`) derive it from that field, so the
    /// value is consistent regardless of which constructor formed the block — it feeds the block
    /// hash, the stake lookup, and the slash-target selection in conflict resolution.
    pub proposer_key: Vec<u8>,
    /// Epoch number at which this block was proposed.
    /// Used by downstream nodes to verify deterministic routing decisions.
    pub epoch_number: u64,
}

impl Block {
    pub fn from_transaction(
        signed: SignedTransaction,
        blockchain: Blockchain,
        token: &Token,
        epoch_number: u64,
    ) -> Self {
        let prev_hash = match blockchain.get_count() {
            // Genesis convention: block 1 has an empty previous_hash.
            0 => Vec::<u8>::new(),
            _ => blockchain.get_current_chain_state().last_hash_in,
        };

        Block {
            signed_trans: signed.clone(),
            token_metadata: token.metadata.clone(),
            previous_hash: prev_hash,
            timestamp: Utc::now().timestamp(),
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            // Phase 2.3 (C2): derive proposer_key from the same SignedTransaction field the
            // other block constructors use, so its value is consistent everywhere it is hashed
            // and consulted for conflict resolution.
            proposer_key: signed.proposer_key.clone(),
            epoch_number,
        }
    }

    pub(crate) fn test_block(prev_hash: Vec<u8>) -> Self {
        let test_transaction = SignedTransaction::test_transaction();
        let mut block = Block {
            signed_trans: test_transaction,
            token_metadata: HashMap::new(),
            previous_hash: prev_hash,
            current_hash: vec![],
            timestamp: Utc::now().timestamp(),
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![],
            epoch_number: 0,
        };

        block.current_hash = BlockFactory::create_hash(&block);
        block
    }
}

pub struct BlockFactory {}

impl BlockFactory {
    /// Create a block hash using SHA-256 from the crypto module.
    ///
    /// Hashes a *canonical* form of the block so that any two equal blocks hash identically
    /// regardless of map insertion order or serialization state (AUDIT finding C2). Input is,
    /// in fixed order, and is hashed as a whole:
    ///
    /// ```text
    /// previous_hash ‖ timestamp ‖ canonical(signed_trans) ‖ canonical(token_metadata)
    ///   ‖ proposer_key ‖ epoch_number
    /// ```
    ///
    /// `signed_trans` and `token_metadata` are `HashMap`-backed, so they are serialized through
    /// sorted-key (BTreeMap) forms that ignore a random-seeded iteration order; `proposer_key`
    /// and `epoch_number` are newly bound into the input.
    pub fn create_hash(block: &Block) -> Vec<u8> {
        let mut input = block.previous_hash.clone();

        let mut time_bytes = crate::encoding::serialize_to_bytes_rmp(&block.timestamp)
            .expect("Block timestamp couldn't be serialized.");
        input.append(&mut time_bytes);

        let mut trans_bytes = crate::transactions::canonical_signed_trans_bytes(&block.signed_trans)
            .expect("Block signed transaction couldn't be canonicalized.");
        input.append(&mut trans_bytes);

        let mut metadata_bytes = canonical_map_bytes(&block.token_metadata)
            .expect("Block token metadata couldn't be canonicalized.");
        input.append(&mut metadata_bytes);

        let mut proposer_bytes = crate::encoding::serialize_to_bytes_rmp(&block.proposer_key)
            .expect("Block proposer_key couldn't be serialized.");
        input.append(&mut proposer_bytes);

        let mut epoch_bytes = crate::encoding::serialize_to_bytes_rmp(&block.epoch_number)
            .expect("Block epoch_number couldn't be serialized.");
        input.append(&mut epoch_bytes);

        BasicHashProvider::new().hash(&input)
    }
}

/// Serialize a `HashMap` in a canonical, sorted-key form.
///
/// A `std` `HashMap`'s iteration order is random-seeded, so serializing the same logical contents
/// directly produces different bytes on different runs. Building a `BTreeMap` from it first sorts
/// the entries by key, making the serialized form deterministic. Used by `create_hash` so a block's
/// hash reflects its content rather than the memory layout of its maps.
fn canonical_map_bytes<'a, K, V>(map: &'a HashMap<K, V>) -> Result<Vec<u8>, std::io::Error>
where
    K: 'a + Ord + Serialize,
    V: 'a + Serialize,
{
    let sorted: BTreeMap<&'a K, &'a V> = map.iter().collect();
    crate::encoding::serialize_to_bytes_rmp(&sorted)
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Blockchain {
    pub chain: VecDeque<Block>,
    /// Metadata about the blockchain (e.g., genesis hash, chain name)
    pub metadata: HashMap<String, String>,
}

/// Outcome of validating and appending a block (AUDIT Phase 3.3 / C5):
/// distinguishes the non-fatal "node is behind" case from the fatal tamper case so callers can keep
/// the existing gossip semantics — drop a stale/sibling block (ignore) but reject an invalid hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The block does not chain onto the current tip (it is a non-tip sibling or the receiver is
    /// behind). The caller should ignore it, never reject.
    LinkageMismatch,
    /// The block's recomputed hash differs from its claimed `current_hash`, or it is internally
    /// inconsistent. The caller must reject it (fail closed).
    InvalidHash,
    /// The block validated and was appended to the tip.
    Appended,
}

impl Blockchain {
    pub fn new() -> Self {
        Blockchain {
            chain: VecDeque::new(),
            metadata: HashMap::new(),
        }
    }

    pub fn get_count(&self) -> usize {
        self.chain.len()
    }

    pub fn add_block(&mut self, block: Block) {
        self.chain.push_back(block);
    }

    /// Validate `block` against this chain and append it, **under a single `&mut self` borrow**
    /// (AUDIT Phase 3.3 / C5).
    ///
    /// The read of the tip (`get_current_chain_state`) and the append (`push_back`) happen inside
    /// this one `&mut self` call — there is no window in which another writer can append a sibling
    /// block between the linkage check and the append. This is the same atomic read-tip-then-append
    /// shape `Token::commit_block` uses on the `Commit` path.
    ///
    /// Linkage (`previous_hash` == tip, or both empty for genesis) is non-fatal: a non-tip sibling
    /// is dropped rather than rejected, preserving the existing gossip semantics. An internally
    /// inconsistent block (recomputed hash != `current_hash`) is fatal.
    pub fn append_validated_block(&mut self, block: &Block) -> AppendOutcome {
        let current_state = self.get_current_chain_state();

        // Linkage (non-fatal): block does not chain onto the current tip — the receiver is behind
        // or this is a sibling block. Drop it, don't reject.
        let linkage_ok = if current_state.last_hash_in.is_empty() {
            block.previous_hash.is_empty()
        } else {
            current_state.last_hash_in == block.previous_hash
        };
        if !linkage_ok {
            return AppendOutcome::LinkageMismatch;
        }

        // Hash (fatal): the block is internally inconsistent — its recomputed hash differs from its
        // claimed `current_hash` (tampered), or its `previous_hash` does not match the recomputed
        // tip. Reject, do not silently pass.
        if BlockFactory::create_hash(block) != block.current_hash {
            return AppendOutcome::InvalidHash;
        }

        self.chain.push_back(block.clone());
        AppendOutcome::Appended
    }

    /// Removes and returns the TIP (most recent block, the head of the chain).
    ///
    /// This is the primitive for rolling back the head of the chain — undoing the most
    /// recently committed block for conflict resolution, orphan handling, or rejecting an
    /// invalid tip. To prune the oldest block instead (bounded-window trimming for
    /// non-archivers), use `remove_oldest`.
    ///
    /// AUDIT Phase 2.4 (finding L7): previously `pop_front()` removed the oldest block and could
    /// never touch the tip, so the chain had no way to undo its head.
    pub fn remove_block(&mut self) -> Option<Block> {
        self.chain.pop_back()
    }

    /// Removes and returns the OLDEST block (genesis side).
    ///
    /// Used to keep the chain at its bounded length for non-archivers: when the chain reaches
    /// `security_level`, the oldest block is pruned and the newest appended. To roll back the
    /// tip instead, use `remove_block`.
    pub fn remove_oldest(&mut self) -> Option<Block> {
        self.chain.pop_front()
    }

    pub fn get_current_chain_state(&self) -> ChainState {
        if self.chain.is_empty() {
            return ChainState::empty();
        }

        let mut prev_block = &self.chain[0];
        let prev_hash = BlockFactory::create_hash(&prev_block);
        let mut valid = prev_block.current_hash == prev_hash;

        for (i, _) in self.chain.iter().enumerate() {
            let next_index = i + 1;
            if !valid {
                return ChainState::invalid();
            } else if self.chain.len() == next_index {
                return ChainState::new(true, prev_block);
            }

            valid = self.chain[next_index].previous_hash == prev_block.current_hash
                && BlockFactory::create_hash(&self.chain[next_index]) == self.chain[next_index].current_hash;

            prev_block = &self.chain[next_index];
        }

        ChainState::invalid()
    }

    pub fn validate_next_block(&self, next_block: &Block) -> bool {
        let current_state = self.get_current_chain_state();
        if !current_state.is_valid {
            return false;
        }

        // Genesis convention: block 1 has an empty previous_hash, matching
        // ChainState::empty().last_hash_in (which is also vec![]).
        let linkage_ok = if current_state.last_hash_in.is_empty() {
            next_block.previous_hash.is_empty()
        } else {
            current_state.last_hash_in == next_block.previous_hash
        };

        linkage_ok && BlockFactory::create_hash(next_block) == next_block.current_hash
    }

    /// Get a block by index. Returns None if out of range.
    pub fn get_block_at(&self, index: usize) -> Option<&Block> {
        self.chain.get(index)
    }

    /// Set the finality status of a block by hash.
    /// Returns Ok(()) if found, Err(()) if block hash not in chain.
    pub fn set_finality_status(
        &mut self,
        block_hash: &[u8],
        status: FinalityStatus,
    ) -> Result<(), ()> {
        for block in self.chain.iter_mut() {
            if block.current_hash == block_hash {
                block.finality_status = status;
                return Ok(());
            }
        }
        Err(())
    }
}

#[derive(Clone)]
pub struct ChainState {
    pub is_valid: bool,
    pub last_hash_in: Vec<u8>,
}

impl ChainState {
    pub fn invalid() -> Self {
        ChainState {
            is_valid: false,
            last_hash_in: vec![],
        }
    }

    pub fn new(valid: bool, last_block: &Block) -> Self {
        ChainState {
            is_valid: valid,
            last_hash_in: last_block.current_hash.clone(),
        }
    }

    pub fn empty() -> Self {
        ChainState {
            is_valid: true,
            last_hash_in: vec![],
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::blocks::Block;
    use crate::transactions::{Transaction, TransactionSignature};

    // --- FinalityStatus tests ---

    #[test]
    fn finality_status_optimistic_deserializes() {
        let status = FinalityStatus::Optimistic;
        let bytes = crate::encoding::serialize_to_bytes_rmp(&status).unwrap();
        let recovered: FinalityStatus = crate::encoding::deserialize_rmp_to(&bytes).unwrap();
        assert_eq!(recovered, FinalityStatus::Optimistic);
    }

    #[test]
    fn finality_status_confirmed_deserializes() {
        let status = FinalityStatus::Confirmed;
        let bytes = crate::encoding::serialize_to_bytes_rmp(&status).unwrap();
        let recovered: FinalityStatus = crate::encoding::deserialize_rmp_to(&bytes).unwrap();
        assert_eq!(recovered, FinalityStatus::Confirmed);
    }

    #[test]
    fn finality_status_block_default_is_optimistic() {
        let tx = SignedTransaction::test_transaction();
        let blockchain = Blockchain::new();
        let token = Token::test_token();
        let block = Block::from_transaction(tx, blockchain, &token, 0);
        assert_eq!(block.finality_status, FinalityStatus::Optimistic);
    }

    #[test]
    fn block_test_block_has_optimistic_finality() {
        let block = Block::test_block(vec![1, 2, 3]);
        assert_eq!(block.finality_status, FinalityStatus::Optimistic);
    }

    #[test]
    fn from_transaction_empty_blockchain_yields_empty_previous_hash() {
        let tx = SignedTransaction::test_transaction();
        let blockchain = Blockchain::new();
        let token = Token::test_token();

        let block = Block::from_transaction(tx, blockchain, &token, 0);

        // Genesis convention: block 1 has an empty previous_hash
        assert!(block.previous_hash.is_empty());
    }

    /// Phase 2.3 (C2): `Block::from_transaction` derives `proposer_key` from the signed
    /// transaction's `proposer_key`, not `leader_address`. A leader/key drift must resolve to a
    /// single, hash-bound value everywhere the block is consumed.
    #[test]
    fn from_transaction_uses_signed_transaction_proposer_key() {
        let mut tx = SignedTransaction::test_transaction();
        // leader_address and proposer_key deliberately diverge (the C2 drift scenario).
        tx.leader_address = vec![11, 22, 33];
        tx.proposer_key = vec![3, 1, 4];
        let blockchain = Blockchain::new();
        let token = Token::test_token();

        let block = Block::from_transaction(tx, blockchain, &token, 7);

        assert_eq!(block.proposer_key, vec![3, 1, 4]);
        assert_eq!(block.epoch_number, 7);
    }

    #[test]
    fn get_current_chain_state_with_empty_chain() {
        let blockchain = Blockchain::new();

        let state = blockchain.get_current_chain_state();

        assert!(state.is_valid);
        assert_eq!(state.last_hash_in.len(), 0);
    }

    #[test]
    fn get_current_chain_state_with_valid_chain() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks to the chain
        let valid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(valid_next_block);

        let state = blockchain.get_current_chain_state();

        assert!(state.is_valid);
        assert!(state.last_hash_in.len() > 0);
    }

    #[test]
    fn get_current_chain_state_with_invalid_chain() {
        let mut blockchain = Blockchain::new();

        // Add some invalid blocks to the chain
        let valid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(valid_next_block);
        let invalid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(invalid_next_block);

        let state = blockchain.get_current_chain_state();

        assert!(!state.is_valid);
        assert_eq!(state.last_hash_in.len(), 0);
    }

    #[test]
    fn validate_next_block_with_valid_block() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks
        let valid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(valid_next_block);

        let last_block_hash = blockchain.get_current_chain_state().last_hash_in;

        let valid_next_block = Block::test_block(last_block_hash);

        assert!(blockchain.validate_next_block(&valid_next_block));
    }

    #[test]
    fn validate_next_block_with_invalid_previous_hash() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks
        let valid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(valid_next_block);

        let invalid_next_block = Block::test_block(vec![1, 2, 3]);

        assert!(!blockchain.validate_next_block(&invalid_next_block));
    }

    #[test]
    fn validate_next_block_with_invalid_block_hash() {
        let mut blockchain = Blockchain::new();

        // Add some valid blocks
        let valid_next_block = Block::test_block(vec![23, 42, 43]);
        blockchain.add_block(valid_next_block);

        let mut invalid_next_block = Block::test_block(
            blockchain.get_current_chain_state().last_hash_in,
        );

        invalid_next_block.current_hash = vec![1, 2, 3];

        assert!(!blockchain.validate_next_block(&invalid_next_block));
    }

    #[test]
    fn validate_next_block_empty_chain_accepts_genesis_block() {
        let blockchain = Blockchain::new();

        // Genesis convention: block 1 has an empty previous_hash
        let genesis_block = Block::test_block(Vec::<u8>::new());

        assert!(blockchain.validate_next_block(&genesis_block));
    }

    #[test]
    fn validate_next_block_empty_chain_rejects_nonempty_previous_hash() {
        let blockchain = Blockchain::new();

        let block = Block::test_block(vec![1, 2, 3]);

        assert!(!blockchain.validate_next_block(&block));
    }

    #[test]
    fn validate_next_block_empty_chain_rejects_invalid_block_hash() {
        let blockchain = Blockchain::new();

        let mut genesis_block = Block::test_block(Vec::<u8>::new());
        genesis_block.current_hash = vec![1, 2, 3];

        assert!(!blockchain.validate_next_block(&genesis_block));
    }

    // --- Phase 2.1 (C2): canonical block hash is order-independent ---

    /// A `TransactionSignature` for an executor public key.
    fn sig_entry(key: Vec<u8>) -> TransactionSignature {
        TransactionSignature {
            transaction_id: vec![1],
            env_id: vec![2],
            transaction_hash: key.clone(),
            signature: key,
            current_stake: 1,
        }
    }

    /// A populated `SignedTransaction` with a non-empty `executor_sigs` map.
    fn signed_tx_with(executor_sigs: HashMap<Vec<u8>, TransactionSignature>) -> SignedTransaction {
        SignedTransaction {
            transaction_id: "block_tx".into(),
            transaction: Transaction {
                id: "block_tx".into(),
                action: "Transfer".into(),
                token_id: vec![1, 2, 3],
                bid: None,
                sequence_number: 7,
                sender: vec![9, 9, 9],
                receiver: vec![8, 8, 8],
                amount: Some(500),
                timestamp: 4242,
                result_hash: vec![0xAA],
                sender_signature: vec![],
            },
            total_stake: 1000,
            total_voters: 5,
            leader_address: vec![11, 22, 33],
            leader_stake: 500,
            leader_hash: vec![4, 5, 6],
            finalizer_addr: vec![7, 7, 7],
            finalizer_sig: sig_entry(vec![7, 7, 7]),
            executor_sigs,
            proposer_key: vec![3, 1, 4],
        }
    }

    /// A populated `Block` whose maps can be varied in insertion order; `current_hash` is computed.
    fn populated_block(
        metadata: HashMap<String, String>,
        executor_sigs: HashMap<Vec<u8>, TransactionSignature>,
    ) -> Block {
        let mut block = Block {
            signed_trans: signed_tx_with(executor_sigs),
            token_metadata: metadata,
            previous_hash: vec![0xDE, 0xAD],
            timestamp: 999,
            current_hash: vec![],
            finality_status: FinalityStatus::Optimistic,
            proposer_key: vec![3, 1, 4],
            epoch_number: 17,
        };
        block.current_hash = BlockFactory::create_hash(&block);
        block
    }

    /// `token_metadata` contents in one order.
    fn metadata_forward() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("decimals".into(), "18".into());
        m.insert("name".into(), "AcmeCoin".into());
        m.insert("symbol".into(), "ACM".into());
        m
    }

    /// Same `token_metadata` contents in a *different* order.
    fn metadata_reversed() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("symbol".into(), "ACM".into());
        m.insert("name".into(), "AcmeCoin".into());
        m.insert("decimals".into(), "18".into());
        m
    }

    /// `executor_sigs` contents in one order.
    fn sigs_forward() -> HashMap<Vec<u8>, TransactionSignature> {
        let mut m = HashMap::new();
        m.insert(vec![10], sig_entry(vec![10]));
        m.insert(vec![20], sig_entry(vec![20]));
        m.insert(vec![30], sig_entry(vec![30]));
        m
    }

    /// Same `executor_sigs` contents in a *different* order.
    fn sigs_reversed() -> HashMap<Vec<u8>, TransactionSignature> {
        let mut m = HashMap::new();
        m.insert(vec![30], sig_entry(vec![30]));
        m.insert(vec![20], sig_entry(vec![20]));
        m.insert(vec![10], sig_entry(vec![10]));
        m
    }

    #[test]
    fn block_hash_is_deterministic_across_insertion_order() {
        let a = populated_block(metadata_forward(), sigs_forward());
        let b = populated_block(metadata_reversed(), sigs_reversed());
        // Same logical block, both maps populated in different insertion orders → identical hash.
        assert_eq!(a.current_hash, b.current_hash);
    }

    #[test]
    fn block_hash_is_deterministic_across_serde_roundtrip() {
        let a = populated_block(metadata_forward(), sigs_forward());
        let bytes = crate::encoding::serialize_to_bytes_rmp(&a).unwrap();
        let mut b: Block = crate::encoding::deserialize_rmp_to(&bytes).unwrap();
        // Recompute as the pipeline does (it sets current_hash = create_hash(block)); the
        // deserialized block carries the same contents in a fresh, re-seeded HashMap state.
        b.current_hash = BlockFactory::create_hash(&b);
        assert_eq!(a.current_hash, b.current_hash);
    }

    #[test]
    fn block_hash_binds_proposer_and_epoch() {
        let a = populated_block(metadata_forward(), sigs_forward());
        // Same everything, only proposer_key differs → hash must change (proposer_key is bound in).
        let mut b = populated_block(metadata_forward(), sigs_forward());
        b.proposer_key = vec![9, 9, 9];
        b.current_hash = BlockFactory::create_hash(&b); // recompute after the field change
        assert_ne!(a.current_hash, b.current_hash);
        // Same everything, only epoch_number differs → hash must change (epoch_number is bound in).
        let mut c = populated_block(metadata_forward(), sigs_forward());
        c.epoch_number = 18;
        c.current_hash = BlockFactory::create_hash(&c); // recompute after the field change
        assert_ne!(a.current_hash, c.current_hash);
    }

    // --- Phase 2.4 (L7): remove_block removes the tip; remove_oldest removes the oldest ---

    #[test]
    fn remove_block_removes_tip_not_oldest() {
        let mut blockchain = Blockchain::new();
        // Build a properly chained multi-block chain: genesis -> b1 -> b2.
        let genesis = Block::test_block(Vec::<u8>::new());
        blockchain.add_block(genesis.clone());
        let b1 = Block::test_block(genesis.current_hash.clone());
        blockchain.add_block(b1.clone());
        let b2 = Block::test_block(b1.current_hash.clone());
        blockchain.add_block(b2.clone());

        assert_eq!(blockchain.get_count(), 3);
        assert!(blockchain.get_current_chain_state().is_valid);

        // `remove_block` must remove the TIP (b2), not the oldest (genesis).
        let removed = blockchain.remove_block().expect("a block to remove");
        assert_eq!(removed.current_hash, b2.current_hash);
        assert_eq!(blockchain.get_count(), 2);

        // The remaining chain (genesis -> b1) is still valid, with b1 now the tip.
        let state = blockchain.get_current_chain_state();
        assert!(state.is_valid);
        assert_eq!(state.last_hash_in, b1.current_hash);

        // `remove_oldest` removes the genesis (oldest) side, leaving b1 as the sole block.
        let oldest = blockchain.remove_oldest().expect("the oldest block");
        assert_eq!(oldest.current_hash, genesis.current_hash);
        assert_eq!(blockchain.get_count(), 1);
        assert_eq!(
            blockchain.get_block_at(0).unwrap().current_hash,
            b1.current_hash
        );
    }
}
