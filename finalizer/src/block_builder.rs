use std::collections::HashMap;
use std::sync::Arc;

use ed25519_dalek::{SigningKey, VerifyingKey, Signer};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use pneumatic_core::blocks::{Block, BlockFactory, FinalityStatus};
use pneumatic_core::crypto::HashProvider;
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::errors::{PneumaticError, ReconciledSignatures};
use pneumatic_core::transactions::{SignedTransaction, Transaction, TransactionSignature};

// ---------------------------------------------------------------------------
// BlockBuilder — forms SignedTransaction and Block from reconciled signatures
// ---------------------------------------------------------------------------

/// Takes reconciled executor signatures and builds a SignedTransaction and Block.
///
/// This module handles:
/// 1. Building the SignedTransaction from reconciled signature data
/// 2. Signing the finalizer's portion of the block
/// 3. Assembling the final Block struct with proper hash chaining
///
/// This does NOT send messages — that is handled by MessageDispatcher.
pub struct BlockBuilder {
    /// Ed25519 signing key for signing blocks as the finalizer
    signing_key: Arc<Mutex<SigningKey>>,
    /// Verifying key for this finalizer (derived from signing key)
    verifying_key: VerifyingKey,
    /// Hash provider for computing block hashes
    hash_provider: Arc<dyn pneumatic_core::crypto::HashProvider>,
    /// The current epoch's verified leader identity — the proposer recorded on the blocks this
    /// finalizer builds. It populates `SignedTransaction.leader_address` and
    /// `SignedTransaction.proposer_key`; both must equal the epoch-selected leader so the final
    /// block's identity is consistent with the committer path (Phase 2.3, finding C2).
    leader_address: Vec<u8>,
    /// Leader's stake amount
    leader_stake: u64,
    /// Leader's genesis block hash
    leader_hash: Vec<u8>,
    /// Finalizer's own public key address
    finalizer_addr: Vec<u8>,
}

impl BlockBuilder {
    /// Create a new BlockBuilder with the given signing key and environment data.
    ///
    /// `signing_key` is the Ed25519 private key used to sign blocks as the finalizer.
    /// `verifying_key` is derived from the signing key and used to produce the finalizer address.
    pub fn new(
        signing_key: SigningKey,
        verifying_key: VerifyingKey,
        hash_provider: Arc<dyn pneumatic_core::crypto::HashProvider>,
        leader_address: Vec<u8>,
        leader_stake: u64,
        leader_hash: Vec<u8>,
        finalizer_addr: Vec<u8>,
    ) -> Self {
        BlockBuilder {
            signing_key: Arc::new(Mutex::new(signing_key)),
            verifying_key,
            hash_provider,
            leader_address,
            leader_stake,
            leader_hash,
            finalizer_addr,
        }
    }

    /// Build a SignedTransaction from reconciled signature data.
    ///
    /// Combines executor signatures into the SignedTransaction's executor_sigs map,
    /// with the finalizer's signature set to a placeholder (to be filled by sign_finalizer_block).
    ///
    /// `total_stake` is the total stake across all voters in the environment.
    /// `total_voters` is the total number of voting nodes.
    pub fn build_signed_transaction(
        &self,
        reconciled: &ReconciledSignatures,
        transaction: &Transaction,
        total_stake: u64,
        total_voters: u32,
    ) -> SignedTransaction {
        // Build executor signatures map from reconciled data
        let executor_sigs: HashMap<Vec<u8>, TransactionSignature> = reconciled
            .executor_signatures
            .iter()
            .map(|es| {
                (
                    es.executor_public_key.clone(),
                    TransactionSignature {
                        transaction_id: transaction.id.as_bytes().to_vec(),
                        env_id: transaction.token_id.clone(),
                        transaction_hash: es.signature.clone(),
                        signature: es.signature.clone(),
                        current_stake: es.stake,
                    },
                )
            })
            .collect();

        // Placeholder finalizer signature — filled by sign_finalizer_block
        let placeholder_sig = TransactionSignature {
            transaction_id: transaction.id.as_bytes().to_vec(),
            env_id: transaction.token_id.clone(),
            transaction_hash: vec![], // filled by sign_finalizer_block
            signature: vec![],
            current_stake: 0,
        };

        SignedTransaction {
            transaction_id: transaction.id.clone(),
            transaction: transaction.clone(),
            total_stake,
            total_voters,
            leader_address: self.leader_address.clone(),
            leader_stake: self.leader_stake,
            leader_hash: self.leader_hash.clone(),
            finalizer_addr: self.finalizer_addr.clone(),
            finalizer_sig: placeholder_sig,
            executor_sigs,
            proposer_key: self.leader_address.clone(),
        }
    }

    /// Sign the finalizer's portion of the block.
    ///
    /// Computes a hash over the transaction + ordered executor signatures,
    /// then signs that hash with the finalizer's private key.
    ///
    /// Returns the finalizer's `TransactionSignature` ready to be embedded
    /// in the SignedTransaction.
    pub async fn sign_finalizer_block(
        &self,
        signed_tx: &mut SignedTransaction,
    ) -> Result<TransactionSignature, PneumaticError> {
        // Hash the transaction bytes
        let tx_bytes = serialize_to_bytes_rmp(&signed_tx)
            .map_err(|e| PneumaticError::Encoding(e.to_string()))?;

        // Hash the ordered executor signatures
        let mut sig_input = Vec::new();
        let mut sig_map: Vec<_> = signed_tx.executor_sigs.iter().collect();
        sig_map.sort_by_key(|(key, _)| key.clone());
        for (_key, sig) in sig_map {
            sig_input.extend_from_slice(&sig.signature);
        }

        // Combined hash: hash(tx_bytes) || hash(sig_input)
        let tx_hash = self.hash_provider.hash(&tx_bytes);
        let sig_hash = self.hash_provider.hash(&sig_input);
        let mut combined_input = tx_hash;
        combined_input.extend_from_slice(&sig_hash);
        let transaction_hash = self.hash_provider.hash(&combined_input);

        // Sign the transaction hash with the finalizer's Ed25519 private key
        let signature = {
            let kp = self.signing_key.lock().await;
            kp.sign(&transaction_hash).to_bytes()
        };

        Ok(TransactionSignature {
            transaction_id: signed_tx.transaction.id.as_bytes().to_vec(),
            env_id: signed_tx.transaction.token_id.clone(),
            transaction_hash: transaction_hash.clone(),
            signature: signature.to_vec(),
            current_stake: self.leader_stake,
        })
    }

    /// Create a Block from a SignedTransaction.
    ///
    /// Computes the block hash using BlockFactory::create_hash and sets
    /// the previous_hash to the current chain state.
    pub fn create_block(
        &self,
        signed_tx: SignedTransaction,
        previous_hash: Vec<u8>,
        epoch_number: u64,
    ) -> Result<Block, PneumaticError> {
        // Phase 2.3 (C2): proposer_key is the block's identity in the hash + conflict resolution,
        // so read it from the signed transaction (the same field `Block::from_transaction` uses).
        let proposer_key = signed_tx.proposer_key.clone();
        let block = Block {
            signed_trans: signed_tx,
            token_metadata: HashMap::new(),
            previous_hash,
            current_hash: vec![],
            timestamp: chrono::Utc::now().timestamp(),
            finality_status: FinalityStatus::Optimistic,
            proposer_key: proposer_key.clone(),
            epoch_number,
        };
        // Compute the block hash.
        // AUDIT Phase 6.9 (Item D): create_hash now returns Result; a serialization failure on the
        // locally-built block surfaces as an error rather than panicking, so this propagates it
        // (this helper now returns Result) instead of expect()-ing message-derived data.
        let current_hash = BlockFactory::create_hash(&block)
            .map_err(|e| PneumaticError::Encoding(format!("create_hash: {e:?}")))?;
        Ok(Block {
            signed_trans: block.signed_trans,
            token_metadata: block.token_metadata,
            previous_hash: block.previous_hash,
            current_hash,
            timestamp: block.timestamp,
            finality_status: FinalityStatus::Optimistic,
            proposer_key,
            epoch_number,
        })
    }

    /// Build a SignedTransaction from a single executor's optimistic signature.
    ///
    /// Used for the fast-path optimistic commit: one executor's honest signature
    /// is proof enough. No reconciliation needed.
    pub fn build_signed_transaction_optimistic(
        &self,
        sig: &TransactionSignature,
        transaction: &Transaction,
        executor_key: &[u8],
    ) -> SignedTransaction {
        // Single executor signature in the map
        let executor_sigs: HashMap<Vec<u8>, TransactionSignature> = [(
            executor_key.to_vec(),
            TransactionSignature {
                transaction_id: transaction.id.as_bytes().to_vec(),
                env_id: transaction.token_id.clone(),
                transaction_hash: sig.signature.clone(),
                signature: sig.signature.clone(),
                current_stake: sig.current_stake,
            },
        )]
        .into_iter()
        .collect();

        // Placeholder finalizer signature — filled by sign_finalizer_block
        let placeholder_sig = TransactionSignature {
            transaction_id: transaction.id.as_bytes().to_vec(),
            env_id: transaction.token_id.clone(),
            transaction_hash: vec![], // filled by sign_finalizer_block
            signature: vec![],
            current_stake: 0,
        };

        SignedTransaction {
            transaction_id: transaction.id.clone(),
            transaction: transaction.clone(),
            total_stake: sig.current_stake,
            total_voters: 1,
            leader_address: self.leader_address.clone(),
            leader_stake: self.leader_stake,
            leader_hash: self.leader_hash.clone(),
            finalizer_addr: self.finalizer_addr.clone(),
            finalizer_sig: placeholder_sig,
            executor_sigs,
            proposer_key: self.leader_address.clone(),
        }
    }

    /// Create a Block with optimistic finality from a SignedTransaction.
    ///
    /// This is an alias for `create_block` — both set `FinalityStatus::Optimistic`.
    pub fn create_block_optimistic(
        &self,
        signed_tx: SignedTransaction,
        previous_hash: Vec<u8>,
        epoch_number: u64,
    ) -> Result<Block, PneumaticError> {
        self.create_block(signed_tx, previous_hash, epoch_number)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pneumatic_core::transactions::Transaction;

    fn make_test_keypair() -> (SigningKey, VerifyingKey) {
        use rand::RngCore;
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();
        (signing_key, verifying_key)
    }

    fn make_test_transaction() -> Transaction {
        Transaction {
            id: "test_tx_001".to_string(),
            action: "Transfer".to_string(),
            token_id: vec![0, 1, 2],
            bid: None,
            sequence_number: 1,
            sender: vec![10, 20, 30],
            receiver: vec![40, 50, 60],
            amount: Some(100),
            timestamp: 1000,
            result_hash: vec![1, 2, 3, 4],
            sender_signature: vec![],
        }
    }

    fn make_reconciled_with_executors() -> ReconciledSignatures {
        ReconciledSignatures {
            executor_signatures: vec![
                pneumatic_core::errors::ExecutorSignature {
                    executor_public_key: b"executor_1".to_vec(),
                    signature: vec![1, 2, 3],
                    stake: 10,
                },
                pneumatic_core::errors::ExecutorSignature {
                    executor_public_key: b"executor_2".to_vec(),
                    signature: vec![4, 5, 6],
                    stake: 20,
                },
            ],
            winning_finalizer: vec![],
            conflict_resolved: false,
        }
    }

    #[test]
    fn test_build_signed_transaction() {
        let hp = Arc::new(pneumatic_core::crypto::BasicHashProvider::new());
        let (signing_key, verifying_key) = make_test_keypair();
        let builder = BlockBuilder::new(
            signing_key,
            verifying_key,
            hp,
            vec![1, 2, 3],
            100,
            vec![4, 5, 6],
            vec![7, 8, 9],
        );

        let tx = make_test_transaction();
        let reconciled = make_reconciled_with_executors();

        let signed = builder.build_signed_transaction(
            &reconciled,
            &tx,
            200,
            3,
        );

        assert_eq!(signed.transaction_id, "test_tx_001");
        assert_eq!(signed.total_stake, 200);
        assert_eq!(signed.total_voters, 3);
        assert_eq!(signed.executor_sigs.len(), 2);
        assert_eq!(signed.finalizer_addr, vec![7, 8, 9]);
        // Placeholder finalizer sig should have empty hash/signature
        assert!(signed.finalizer_sig.signature.is_empty());
    }

    #[tokio::test]
    async fn test_sign_finalizer_block() {
        let hp = Arc::new(pneumatic_core::crypto::BasicHashProvider::new());
        let (signing_key, verifying_key) = make_test_keypair();
        let builder = BlockBuilder::new(
            signing_key,
            verifying_key,
            hp,
            vec![1, 2, 3],
            100,
            vec![4, 5, 6],
            vec![7, 8, 9],
        );

        let tx = make_test_transaction();
        let reconciled = make_reconciled_with_executors();

        let mut signed = builder.build_signed_transaction(
            &reconciled,
            &tx,
            200,
            3,
        );

        let finalizer_sig = builder.sign_finalizer_block(&mut signed).await.unwrap();

        assert!(!finalizer_sig.signature.is_empty());
        assert!(!finalizer_sig.transaction_hash.is_empty());
        assert_eq!(finalizer_sig.current_stake, 100);

        // Update the placeholder in the signed transaction
        signed.finalizer_sig = finalizer_sig;
        assert!(!signed.finalizer_sig.signature.is_empty());
        assert!(!signed.finalizer_sig.transaction_hash.is_empty());
    }

    #[test]
    fn test_create_block() {
        let hp = Arc::new(pneumatic_core::crypto::BasicHashProvider::new());
        let (signing_key, verifying_key) = make_test_keypair();
        let builder = BlockBuilder::new(
            signing_key,
            verifying_key,
            hp,
            vec![1, 2, 3],
            100,
            vec![4, 5, 6],
            vec![7, 8, 9],
        );

        let tx = make_test_transaction();
        let reconciled = make_reconciled_with_executors();
        let mut signed = builder.build_signed_transaction(
            &reconciled,
            &tx,
            200,
            3,
        );
        // Sign before creating block
        let finalizer_sig = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { builder.sign_finalizer_block(&mut signed).await.unwrap() });
        signed.finalizer_sig = finalizer_sig;

        let block = builder.create_block(signed, vec![10, 20, 30], 0)
            .expect("well-formed test block hash");

        assert!(!block.current_hash.is_empty());
        assert_eq!(block.previous_hash, vec![10, 20, 30]);
        assert_eq!(block.signed_trans.transaction_id, "test_tx_001");
    }

    #[test]
    fn test_executor_sigs_sorted_by_key() {
        let hp = Arc::new(pneumatic_core::crypto::BasicHashProvider::new());
        let (signing_key, verifying_key) = make_test_keypair();
        let builder = BlockBuilder::new(
            signing_key,
            verifying_key,
            hp,
            vec![],
            0,
            vec![],
            vec![],
        );

        // Reconcile with unsorted keys
        let reconciled = ReconciledSignatures {
            executor_signatures: vec![
                pneumatic_core::errors::ExecutorSignature {
                    executor_public_key: b"beta".to_vec(),
                    signature: vec![1],
                    stake: 10,
                },
                pneumatic_core::errors::ExecutorSignature {
                    executor_public_key: b"alpha".to_vec(),
                    signature: vec![2],
                    stake: 20,
                },
            ],
            winning_finalizer: vec![],
            conflict_resolved: false,
        };

        let tx = make_test_transaction();
        let signed = builder.build_signed_transaction(&reconciled, &tx, 0, 0);

        // Both executor keys should be present
        assert_eq!(signed.executor_sigs.len(), 2);
        let alpha_key: Vec<u8> = b"alpha".to_vec();
        let beta_key: Vec<u8> = b"beta".to_vec();
        assert!(signed.executor_sigs.contains_key(&alpha_key));
        assert!(signed.executor_sigs.contains_key(&beta_key));
    }

    /// Phase 2.3 (C2): the three block constructors agree on `proposer_key` from a
    /// `SignedTransaction` whose `leader_address` and `proposer_key` diverge. Without the
    /// `Block::from_transaction` fix (which read `leader_address`), the core block would carry a
    /// different, hash-bound proposer identity than the finalizer block.
    #[test]
    fn all_block_constructors_agree_on_proposer_key() {
        let signed = SignedTransaction {
            transaction_id: "c2_test".into(),
            transaction: Transaction {
                id: "c2_test".into(),
                action: "Transfer".into(),
                token_id: vec![1, 2, 3],
                bid: None,
                sequence_number: 1,
                sender: vec![9],
                receiver: vec![8],
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
            finalizer_sig: TransactionSignature {
                transaction_id: vec![1],
                env_id: vec![2],
                transaction_hash: vec![3],
                signature: vec![4],
                current_stake: 1,
            },
            executor_sigs: HashMap::new(),
            proposer_key: vec![3, 1, 4],
        };

        // Finalizer: BlockBuilder::create_block.
        let hp = Arc::new(pneumatic_core::crypto::BasicHashProvider::new());
        let (signing_key, verifying_key) = make_test_keypair();
        let builder = BlockBuilder::new(
            signing_key,
            verifying_key,
            hp,
            vec![11, 22, 33], // leader_address
            500,
            vec![4, 5, 6],
            vec![7, 7, 7],
        );
        let finalizer_block = builder.create_block(signed.clone(), vec![1, 2, 3], 17)
            .expect("well-formed test block hash");
        assert_eq!(finalizer_block.proposer_key, vec![3, 1, 4]);

        // Core: Token::create_block.
        let token_block = pneumatic_core::tokens::Token::new().create_block(signed.clone(), 17);
        assert_eq!(token_block.proposer_key, vec![3, 1, 4]);

        // Core: Block::from_transaction — the constructor the fix changed.
        let token = pneumatic_core::tokens::Token::new();
        let blockchain = pneumatic_core::blocks::Blockchain::new();
        let core_block = pneumatic_core::blocks::Block::from_transaction(
            signed,
            blockchain,
            &token,
            17,
        );
        assert_eq!(core_block.proposer_key, vec![3, 1, 4]);
    }
}
