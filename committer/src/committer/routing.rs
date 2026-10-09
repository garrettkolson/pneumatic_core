//! ADR-004 enforcement on the Commit path: a block enters the chain only if the
//! Finalizer the Sentinel routed its transaction to is the node that sent it.
//!
//! This is the check that decides whether a mis-routed transaction becomes a fork
//! in the history. Everything upstream can be bypassed or misconfigured — the
//! Committer is the only role that appends, so "one Finalizer per transaction" has
//! to be load-bearing *here* even if it is honoured everywhere else.
//!
//! The hole it closes is not a missing signature check. `verify_block_finalizer_sig`
//! has verified a finalizer signature since Phase 3.3 (C5) — against
//! `SignedTransaction::finalizer_addr`, a public key taken **from the block being
//! verified**, which makes it tautological: a block that names an attacker's key and
//! carries that key's signature passes. And it was wired only to `BlockFinalized`,
//! the gossip path, so the `Commit` path — the one that actually appends — never
//! checked a finalizer signature at all. Its only identity gate was role
//! membership: any registered Finalizer could commit any transaction, which is why
//! four hosts finalizing one transaction produced four candidate blocks and the
//! commit layer silently kept whichever arrived first.
//!
//! What is verified, and why each part is needed:
//!
//! 1. **The transaction carries a routing decision.** A transaction reaching the
//!    Commit path without one was not routed. The leader-proposal path does not
//!    come through here (it calls `check_and_commit_transaction_results` directly
//!    from the epoch loop), so unlike the self-verified-token case at the Sentinel,
//!    `None` on this path has no innocent explanation.
//! 2. **The attestation verifies, and the attester is a registered Sentinel.**
//!    Signature verification alone is vacuous — anyone can sign a record with their
//!    own key and have it check out. The key must resolve to a role the protocol
//!    trusts to route (ADR-007).
//! 3. **The authenticated sender is the assigned Finalizer.** `sender_key` is
//!    `message.public_key`, already proven by the envelope signature in
//!    `authenticate_message` — unlike `finalizer_addr`, it is not a value the block
//!    supplies about itself.
//! 4. **The block's finalizer signature verifies under the assigned key**, not under
//!    the key the block claims. Same bytes as C5, different anchor, and now applied
//!    on the path that appends.
//!
//! H12 (the payload hash-compare) covers this field for free: `selection` is part of
//! `Transaction`, so the whole-struct hash the committer compares already binds the
//! routing decision to the transaction that was validated and pooled. A block cannot
//! arrive with a swapped-in decision while still matching the pooled payload.
//!
//! Not verified here, and named as the residual: that the assignment is the one
//! `deterministic_select` would produce for this epoch and salt. The record carries
//! `epoch_number` and the ADR-019 `salt` precisely so that check can exist, but the
//! committer has no epoch-keyed stake-snapshot read (only a current-epoch cache),
//! and a tip that has moved one block selects a different node — so re-deriving on
//! the commit path would reject honest blocks. That belongs with Phase 7's
//! "declare and verify the responsible set".

use super::*;
use pneumatic_core::epoch::FINALIZER_DOMAIN;

impl Committer {
    /// Refuse a `Commit` this node has no reason to accept. Called before any state
    /// is touched — no materialization, no lock, no chain append.
    pub(crate) fn commit_authority(
        &self,
        commit: &TransactionCommit,
        sender_key: &[u8],
    ) -> Result<(), CommitterError> {
        self.block_authority(
            &commit.proposed_block,
            sender_key,
            &String::from_utf8_lossy(&commit.trans_id),
        )
    }

    /// The same check for the inbound `BlockFinalized` path, which appends a block
    /// exactly as `Commit` does. It used to end at `verify_block_finalizer_sig` — a
    /// signature check anchored on a key the block supplied about itself, so a block
    /// naming an attacker's key and carrying that key's signature passed it.
    ///
    /// `BlockFinalized` is admitted from the `Finalizer` role only
    /// (`Committer::allowed_senders_for`), and the finalizer that finalized a
    /// transaction is the one that signs it, so the sender-naming check applies here
    /// as it does on the `Commit` path.
    pub(crate) fn gossip_authority(
        &self,
        block: &Block,
        sender_key: &[u8],
    ) -> Result<(), CommitterError> {
        let tx_id = block.signed_trans.transaction.id.clone();
        self.block_authority(block, sender_key, &tx_id)
    }

    /// One transaction, one owner: the block may be appended only if the Sentinel's
    /// routing decision names the node that sent it.
    fn block_authority(
        &self,
        block: &Block,
        sender_key: &[u8],
        tx_id: &str,
    ) -> Result<(), CommitterError> {
        let tx = &block.signed_trans.transaction;

        let Some(record) = tx.selection.as_ref() else {
            // No routing decision. The `Commit` path is the Finalizer path, and every
            // transaction that enters the routed pipeline carries one — so absent
            // here means absent from the pipeline.
            //
            // ONE exemption, and it is a known gap rather than an interpretation:
            // a shielded block's `Transaction` is the S3.1 *placeholder*, built by
            // `build_signed_transaction_shielded` around a `ShieldedTransaction` that
            // has no `selection` field. Adding one means touching the canonical
            // surface the ZK binding rides on, which is a separate decision — so
            // shielded blocks keep their pre-ADR-004 treatment (role-gated sender,
            // finalizer signature verified against the key the block declares) and
            // are named as the remaining hole on this path rather than silently
            // refused or silently treated as routed.
            if block.signed_trans.shielded.is_some() {
                self.logger().log(format!(
                    "ADR-004: shielded block for tx {} has no routing decision to check — \
                     routing is enforced for shielded finalization by delivery only, not by \
                     refusal (see decision-deterministic-per-tx-routing)",
                    tx_id
                ));
                return self.verify_block_finalizer_sig(block);
            }
            return Err(CommitterError::Routing(format!(
                "block for tx {} carries no ADR-004 routing decision; the Commit path is the \
                 Finalizer path, so a block whose transaction was never routed is refused \
                 rather than accepted from whichever registered Finalizer sent it first",
                tx_id
            )));
        };

        // (2) The decision must be the Sentinel's, and the signer must hold the
        // Sentinel role. Both halves or neither means anything.
        let attested = record
            .verify(&tx.id, FINALIZER_DOMAIN, &self.identity.ed25519)
            .map_err(CommitterError::Core)?;
        let sentinel_hex = hex(&record.sentinel_public_key);
        if !attested {
            return Err(CommitterError::Routing(format!(
                "routing decision for tx {} does not verify under its claimed attester {}",
                tx_id, sentinel_hex
            )));
        }
        let roles = self.node_registry.find_node_types_by_public_key(&record.sentinel_public_key);
        if !roles.contains(&NodeRegistryType::Sentinel) {
            return Err(CommitterError::Routing(format!(
                "routing decision for tx {} was attested by {}, which is not a registered \
                 Sentinel (roles: {:?})",
                tx_id, sentinel_hex, roles
            )));
        }

        // (3) The sender — authenticated by the envelope, not by the block — must be
        // the Finalizer the decision names.
        if !record.names(sender_key) {
            return Err(CommitterError::Routing(format!(
                "tx {} is assigned to Finalizer {}, but this block came from {}; refusing a \
                 block from a node the routing decision does not name",
                tx_id,
                hex(&record.finalizer_public_key),
                hex(sender_key)
            )));
        }

        // (4) The block's finalizer signature, checked under the assigned key rather
        // than the key the block declares about itself.
        let finalizer_sig = &block.signed_trans.finalizer_sig;
        if finalizer_sig.signature.is_empty() {
            return Err(CommitterError::InvalidFinalizerSignature);
        }
        let valid = self.identity.ed25519.check_signature(
            &finalizer_sig.signature,
            &record.finalizer_public_key,
            &finalizer_sig.transaction_hash,
        )?;
        if !valid {
            return Err(CommitterError::InvalidFinalizerSignature);
        }

        Ok(())
    }
}

/// Lowercase hex for refusal messages — an operator greps these against a node's
/// own key, so the same casing the rest of the logs use.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
