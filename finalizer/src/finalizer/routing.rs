//! ADR-004 routing enforcement on the Finalizer side: this node may act on a
//! transaction only if the Sentinel's routing decision names *this* node.
//!
//! The check has three parts, and each one closes a different hole:
//!
//! 1. **A decision must exist.** `None` is not "unconstrained". A transaction that
//!    reached a Finalizer without a routing decision did not come from the routed
//!    path, and the historical behaviour when nothing named an owner was for every
//!    host running a Finalizer to take ownership — one block per host for one
//!    transaction (`fact-composite-per-host-fork-divergence`).
//! 2. **The decision must be the Sentinel's.** Signature verification alone proves
//!    nothing: any sender can sign a record with its own key and have the signature
//!    check out. What makes the attestation mean something is that the key it was
//!    signed with resolves, in the node registry, to a registered **Sentinel**
//!    (ADR-007: the Sentinel is the routing authority). Both halves are required;
//!    dropping either makes the other decorative.
//! 3. **The decision must name this node.** Otherwise the transaction belongs to a
//!    different Finalizer, and finalizing it here is the fork.
//!
//! What this deliberately does *not* do is re-derive the selection from the stake
//! snapshot. That needs the snapshot for the record's epoch and the chain tip that
//! was current when the selection was made — the record carries both (`epoch_number`,
//! `salt`) precisely so the check can exist, but a Finalizer reads only the current
//! epoch's set (`get_stake_set_for_epoch`), and a tip one block off selects a
//! different node, so re-deriving here would reject honest work rather than forgery.
//! Derivation-verification belongs with Phase 7's "declare and verify the
//! responsible set", which is also where the epoch-keyed snapshot read lands.

use super::*;
use pneumatic_core::epoch::FINALIZER_DOMAIN;

impl Finalizer {
    /// Refuse a transaction this node has no authority to finalize.
    ///
    /// Every inbound path that can lead to a block — preload registration, the
    /// executor's vote, and both finalize paths — passes through here, so a
    /// transaction assigned elsewhere cannot produce a block on this host by any
    /// route, including the shared-registry route a composite host opens where the
    /// entry is already present because this host's own Sentinel role registered it.
    pub(crate) fn finalize_authority(&self, tx: &Transaction) -> Result<(), PneumaticError> {
        let record = tx.selection.as_ref().ok_or_else(|| {
            PneumaticError::Routing(format!(
                "tx {} has no ADR-004 routing decision; a transaction with no assigned \
                 Finalizer is refused rather than finalized by every host that has one",
                tx.id
            ))
        })?;

        let attested = record.verify(&tx.id, FINALIZER_DOMAIN, &self.identity.ed25519)?;
        if !attested {
            return Err(PneumaticError::Routing(format!(
                "routing decision for tx {} does not verify under {}",
                tx.id,
                bytes_to_hex(&record.sentinel_public_key)
            )));
        }

        // The attester must hold the Sentinel role. Without this, the check above
        // would pass for a record signed by the sender's own key — which is to say,
        // for a forgery.
        let roles = self.node_registry.find_node_types_by_public_key(&record.sentinel_public_key);
        if !roles.contains(&NodeRegistryType::Sentinel) {
            return Err(PneumaticError::Routing(format!(
                "routing decision for tx {} was attested by {}, which is not a registered \
                 Sentinel (roles: {:?})",
                tx.id,
                bytes_to_hex(&record.sentinel_public_key),
                roles
            )));
        }

        // The decision must name this node.
        if !record.names(&self.public_key) {
            return Err(PneumaticError::Routing(format!(
                "tx {} is assigned to Finalizer {}, not this node ({})",
                tx.id,
                bytes_to_hex(&record.finalizer_public_key),
                bytes_to_hex(&self.public_key)
            )));
        }

        Ok(())
    }

    /// Same check, resolved by transaction id from this finalizer's registry.
    ///
    /// Used where the handler holds only an id — the executor's vote names a
    /// transaction, and the routing decision lives on the entry its preload
    /// registered.
    ///
    /// **A missing entry is not a refusal here.** That is a deliberate asymmetry with
    /// the other two gates, not an oversight. Preload and the finalize path are where
    /// a decision has to exist: preload is where this node takes ownership, and the
    /// finalize path is where a block is made. A vote arriving for a transaction that
    /// was never preloaded cannot produce a block — the optimistic path loads the
    /// entry and fails without it — so refusing it at intake would replace a precise,
    /// already-tested failure ("not in registry", the premise of the C1 voter-auth
    /// suite) with a different failure for the same outcome. The gate below therefore
    /// refuses what it can *prove* is not ours — an entry that names another Finalizer
    /// — and leaves the missing-entry case to the choke point that already handles it.
    pub(crate) fn finalize_authority_for_id(&self, tx_id: &str) -> Result<(), PneumaticError> {
        // `get_transaction` is deliberately not used here: it only serves the
        // Validated state, and a transaction reaches a vote while Preloaded (from
        // its own preload) or Finalizing (the composite's shared registry, where
        // this host's executor role already stamped it).
        let entry = match self.pending_registry.get_transaction_mut(tx_id) {
            Ok(entry) => entry,
            Err(_) => return Ok(()),
        };
        let tx = match &entry.state {
            TransactionState::Preloaded { transaction }
            | TransactionState::Validated { transaction, .. }
            | TransactionState::Executing { transaction, .. }
            | TransactionState::Finalizing { transaction, .. } => transaction.clone(),
            // A terminal state (Committed/Failed) is not this node's to finalize
            // either, and the finalize path refuses it on state grounds; nothing here
            // can turn it into a block.
            _ => return Ok(()),
        };
        drop(entry);
        self.finalize_authority(&tx)
    }
}
