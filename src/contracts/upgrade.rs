//! Upgrade governance (ADR-017, Phase 8).
//!
//! An `UpgradeContract` tx swaps a contract token's `SmartContract` asset (the
//! bytecode) under **M-of-N owner signatures** plus a mandatory **one-epoch
//! timelock**. It is engine-agnostic — it swaps a `Spec` rmp-AST or a `Wasm`
//! module alike (the frozen ABI, ADR-018, guarantees compatibility).
//!
//! The executor stays **pure** (ADR-013): it re-validates the quorum
//! deterministically and emits a canonical [`ReplaceAssetDelta`] in
//! `result_data`; the data service (committer) applies it at commit — and only
//! once the timelock has elapsed.
//!
//! Consensus-critical pieces pinned here (changing them is a hard fork):
//! - the [`UPGRADE_DOMAIN`] + [`upgrade_digest`] formula (QD2);
//! - the [`ReplaceAssetDelta`] schema;
//! - the quorum rule ([`verify_quorum`]);
//! - the timelock rule ([`TIMELOCK_EPOCHS`] / [`timelock_satisfied`]);
//! - the upgrade gas formula ([`upgrade_gas`]).

use serde::Serialize;

use crate::crypto::{AsymCryptoProvider, HashProvider};
use crate::encoding::serialize_to_bytes_rmp;
use crate::tokens::{SmartContract, Token};

use super::ContractError;

/// Domain tag for the canonical upgrade digest (collision-resistant, versioned).
const UPGRADE_DOMAIN: &[u8] = b"PNEUMATIC/UPGRADE/v1";

/// Upgrade gas: a flat base plus a per-byte cost of the new bytecode.
pub const UPGRADE_GAS_BASE: u64 = 50_000;
/// Per-byte upgrade gas (same shape as `deploy_gas`).
pub const UPGRADE_GAS_PER_BYTE: u64 = 10;
/// The timelock delay in epochs: an upgrade applies at `proposal_epoch + 1` or later.
pub const TIMELOCK_EPOCHS: u64 = 1;

/// Upgrade parameters carried in an `UpgradeContract` tx `payload` (rmp-canonical).
///
/// The `sender`, `nonce`, and `gas_limit` come from the tx envelope and the sender's
/// `User` state, not here. The target is `tx.token_id` (unlike a deploy, the target
/// contract already exists).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct UpgradeParams {
    /// The new contract bytecode (a `Spec` rmp-AST or a `Wasm` module).
    pub new_bytecode: Vec<u8>,
    /// The new owner set (Ed25519 verifying keys). Empty + threshold 0 == immutable.
    pub new_owners: Vec<Vec<u8>>,
    /// The new M-of-N threshold.
    pub new_threshold: u32,
    /// The epoch at which the upgrade was proposed (the timelock anchor, QD3).
    pub proposal_epoch: u64,
    /// Owner signatures over the canonical upgrade digest (QD2).
    pub owner_signatures: Vec<Vec<u8>>,
}

/// The canonical `ReplaceAsset` state delta the executor emits in `result_data`.
///
/// Because the new bytecode + owner set ride in the tx `payload`, the committer can
/// **re-derive** this delta from the transaction (exactly as it re-derives the deploy
/// `CreateTokenDelta`) and verify it against the signed `result_hash`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ReplaceAssetDelta {
    /// The target token id.
    pub token_id: Vec<u8>,
    /// The new contract bytecode.
    pub new_bytecode: Vec<u8>,
    /// The new owner set.
    pub new_owners: Vec<Vec<u8>>,
    /// The new M-of-N threshold.
    pub new_threshold: u32,
    /// The proposal epoch (timelock anchor).
    pub proposal_epoch: u64,
}

/// Upgrade gas cost for a new bytecode of `bytecode_len` bytes.
///
/// `upgrade_gas = UPGRADE_GAS_BASE + UPGRADE_GAS_PER_BYTE * bytecode_len`.
pub fn upgrade_gas(bytecode_len: usize) -> u64 {
    UPGRADE_GAS_BASE + UPGRADE_GAS_PER_BYTE * (bytecode_len as u64)
}

/// The canonical upgrade digest — the message owners sign (QD2). A pure function of
/// the upgrade inputs, identical on every shard member:
///
/// ```text
/// upgrade_digest = SHA256( UPGRADE_DOMAIN
///                  ‖ rmp_canon( token_id,
///                               SHA256(new_bytecode),
///                               new_owners,
///                               new_threshold,
///                               proposal_epoch ) )
/// ```
///
/// `new_owners` serializes in rmp-canonical order (the delta's owner set). Changing
/// any input — the bytecode, the owner set, the threshold, the proposal epoch — yields
/// a different digest, so a signature for one upgrade does not authorize another.
pub fn upgrade_digest(
    token_id: &[u8],
    new_bytecode: &[u8],
    new_owners: &Vec<Vec<u8>>,
    new_threshold: u32,
    proposal_epoch: u64,
    hash: &dyn HashProvider,
) -> Vec<u8> {
    let bytecode_hash = hash.hash(new_bytecode);
    // A local tuple-of-fields for a canonical rmp serialization of the digest inputs.
    #[derive(Serialize)]
    struct DigestInput<'a> {
        token_id: &'a [u8],
        bytecode_hash: Vec<u8>,
        new_owners: &'a Vec<Vec<u8>>,
        new_threshold: u32,
        proposal_epoch: u64,
    }
    let input = DigestInput {
        token_id,
        bytecode_hash,
        new_owners,
        new_threshold,
        proposal_epoch,
    };
    let body = serialize_to_bytes_rmp(&input).expect("canonical rmp serialization");
    let mut with_domain = Vec::with_capacity(UPGRADE_DOMAIN.len() + body.len());
    with_domain.extend_from_slice(UPGRADE_DOMAIN);
    with_domain.extend_from_slice(&body);
    hash.hash(&with_domain)
}

/// Verify the M-of-N quorum (QD2): count the number of **distinct current owners**
/// whose signature verifies against the canonical upgrade digest; the upgrade is
/// authorized iff that count `>= threshold`.
///
/// `current_owners` / `threshold` are the target token's *current* owner set and
/// threshold (from its current `SmartContract`), **not** the proposed `new_owners`.
/// Using the current owners is what makes owner-set rotation safe: a proposed owner
/// set can only take over if the *current* quorum authorized it.
///
/// A `threshold` of `0` (an immutable contract) always returns `false`. A malformed
/// signature simply does not count (fail closed).
pub fn verify_quorum(
    token_id: &[u8],
    new_bytecode: &[u8],
    new_owners: &Vec<Vec<u8>>,
    new_threshold: u32,
    provider_epoch: u64,
    owner_signatures: &Vec<Vec<u8>>,
    current_owners: &Vec<Vec<u8>>,
    threshold: u32,
    crypto: &dyn AsymCryptoProvider,
    hash: &dyn HashProvider,
) -> Result<bool, ContractError> {
    if threshold == 0 {
        return Ok(false);
    }
    let digest =
        upgrade_digest(token_id, new_bytecode, new_owners, new_threshold, provider_epoch, hash);
    let mut verified = 0usize;
    for owner in current_owners {
        let owner_ok = owner_signatures
            .iter()
            .any(|sig| crypto.check_signature(sig, owner, &digest).unwrap_or(false));
        if owner_ok {
            verified += 1;
            if (verified as u32) >= threshold {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Whether the one-epoch timelock (QD3) is satisfied for a proposal made at
/// `proposal_epoch`, applied in a block at `current_epoch`:
///
/// `timelock_satisfied ⟺ current_epoch >= proposal_epoch + TIMELOCK_EPOCHS`.
pub fn timelock_satisfied(proposal_epoch: u64, current_epoch: u64) -> bool {
    current_epoch >= proposal_epoch.saturating_add(TIMELOCK_EPOCHS)
}

/// Apply a [`ReplaceAssetDelta`] to a token: replace its `SmartContract` asset's
/// bytecode + owner set. The contract's `name`, `version`, and `storage` are
/// **preserved** (the upgrade swaps the module under the frozen ABI; state carries
/// over).
///
/// Returns the mutated contract. Fails if the token has no contract asset (it is not
/// a contract). Idempotent in the sense that applying the same delta twice yields the
/// same asset.
pub fn apply_replace_asset(
    token: &mut Token,
    delta: &ReplaceAssetDelta,
) -> Result<SmartContract, ContractError> {
    let contract: SmartContract = token
        .update_asset(|c: &mut SmartContract| {
            c.bytecode = delta.new_bytecode.clone();
            c.owners = delta.new_owners.clone();
            c.threshold = delta.new_threshold;
        })
        .ok_or_else(|| {
            ContractError::BadBytecode("target token has no contract asset".to_string())
        })?;
    Ok(contract)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{BasicHashProvider, Ed25519Provider, HashProvider};

    fn hash() -> BasicHashProvider {
        BasicHashProvider::new()
    }

    /// A deterministic 32-byte pseudo-key (not a real keypair — used only to make
    /// the digest inputs distinct; real signing is exercised in the quorum tests).
    fn fake_key(seed: u8) -> Vec<u8> {
        (0..32).map(|i| seed.wrapping_add(i as u8)).collect()
    }

    #[test]
    fn upgrade_gas_formula() {
        assert_eq!(upgrade_gas(0), UPGRADE_GAS_BASE);
        assert_eq!(upgrade_gas(1000), UPGRADE_GAS_BASE + 10 * 1000);
    }

    #[test]
    fn digest_is_deterministic_and_domain_separated() {
        let h = hash();
        let a = upgrade_digest(b"tok", b"bc", &vec![fake_key(1)], 2, 7, &h);
        let b = upgrade_digest(b"tok", b"bc", &vec![fake_key(1)], 2, 7, &h);
        assert_eq!(a, b, "same inputs → same digest");

        // A different bytecode → different digest (a signature for one upgrade does
        // not authorize another).
        let c = upgrade_digest(b"tok", b"bc2", &vec![fake_key(1)], 2, 7, &h);
        assert_ne!(a, c);
        // A different proposal epoch → different digest.
        let d = upgrade_digest(b"tok", b"bc", &vec![fake_key(1)], 2, 8, &h);
        assert_ne!(a, d);
        // A different owner set → different digest.
        let e = upgrade_digest(b"tok", b"bc", &vec![fake_key(9)], 2, 7, &h);
        assert_ne!(a, e);
    }

    #[test]
    fn quorum_threshold_zero_is_immutable() {
        let h = hash();
        let crypto = Ed25519Provider::generate();
        let ok = verify_quorum(
            b"tok",
            b"bc",
            &vec![fake_key(1)],
            1,
            7,
            &vec![],
            &vec![fake_key(1)],
            0,
            &crypto,
            &h,
        )
        .unwrap();
        assert!(!ok, "threshold 0 == no one can upgrade");
    }

    #[test]
    fn quorum_edges_m_minus_1_rejected_m_accepted() {
        // Three real owner keypairs, threshold 2.
        let crypto = Ed25519Provider::generate();
        let h = hash();
        let owners: Vec<(Vec<u8>, Ed25519Provider)> = (0..3)
            .map(|_| {
                let p = Ed25519Provider::generate();
                let pk = p.public_key().unwrap();
                (pk, p)
            })
            .collect();
        let current_owners: Vec<Vec<u8>> = owners.iter().map(|(pk, _)| pk.clone()).collect();

        let token_id = b"tok";
        let new_bytecode = b"new-bc";
        let new_owners = vec![fake_key(9)];
        let new_threshold = 1u32;
        let proposal_epoch = 7u64;
        let digest = upgrade_digest(token_id, new_bytecode, &new_owners, new_threshold, proposal_epoch, &h);

        // Sign with only the first two owners (M-1 of threshold 2? no — 2 == M).
        // Build the M-1 case: threshold 2, only 1 signature.
        let sigs_m1 = vec![owners[0].1.sign_data(&digest).unwrap()];
        let ok_m1 = verify_quorum(
            token_id, new_bytecode, &new_owners, new_threshold, proposal_epoch,
            &sigs_m1, &current_owners, 2, &crypto, &h,
        )
        .unwrap();
        assert!(!ok_m1, "M-1 signatures (1 of 2) must be rejected");

        // Sign with two owners (M == threshold 2).
        let sigs_m = vec![
            owners[0].1.sign_data(&digest).unwrap(),
            owners[1].1.sign_data(&digest).unwrap(),
        ];
        let ok_m = verify_quorum(
            token_id, new_bytecode, &new_owners, new_threshold, proposal_epoch,
            &sigs_m, &current_owners, 2, &crypto, &h,
        )
        .unwrap();
        assert!(ok_m, "M signatures (2 of 2) must be accepted");
    }

    #[test]
    fn quorum_uses_current_owners_not_proposed() {
        // A signature by a key that is NOT in the current owner set must not count.
        let crypto = Ed25519Provider::generate();
        let h = hash();
        let current = Ed25519Provider::generate();
        let current_pk = current.public_key().unwrap();
        let outsider = Ed25519Provider::generate();
        let outsider_pk = outsider.public_key().unwrap();

        let token_id = b"tok";
        let new_bytecode = b"new-bc";
        let new_owners = vec![outsider_pk.clone()];
        let new_threshold = 1u32;
        let proposal_epoch = 7u64;
        let digest = upgrade_digest(token_id, new_bytecode, &new_owners, new_threshold, proposal_epoch, &h);

        // The outsider signs, but the current owner set is just `current` (threshold 1).
        // The outsider's signature does not verify against `current_pk`, so quorum fails.
        let sigs = vec![outsider.sign_data(&digest).unwrap()];
        let ok = verify_quorum(
            token_id, new_bytecode, &new_owners, new_threshold, proposal_epoch,
            &sigs, &vec![current_pk], 1, &crypto, &h,
        )
        .unwrap();
        assert!(!ok, "a signature by a non-current owner must not count toward quorum");
    }

    #[test]
    fn timelock_gate() {
        // A proposal at epoch 5 applies at epoch 6 or later, not at epoch 5.
        assert!(!timelock_satisfied(5, 5), "same epoch is too early");
        assert!(timelock_satisfied(5, 6), "epoch + 1 is in time");
        assert!(timelock_satisfied(5, 10), "much later is in time");
        // saturating: proposal_epoch = u64::MAX does not overflow.
        assert!(timelock_satisfied(u64::MAX, u64::MAX), "no overflow on the max epoch");
    }

    #[test]
    fn apply_replace_asset_swaps_bytecode_and_preserves_state() {
        let mut token = Token::new();
        let initial = SmartContract {
            name: "c".to_string(),
            bytecode: b"old".to_vec(),
            version: "1".to_string(),
            storage: {
                let mut m = std::collections::BTreeMap::new();
                m.insert(b"k".to_vec(), b"v".to_vec());
                m
            },
            owners: vec![fake_key(1)],
            threshold: 1,
        };
        token.set_asset(&initial).unwrap();

        let delta = ReplaceAssetDelta {
            token_id: b"tok".to_vec(),
            new_bytecode: b"new".to_vec(),
            new_owners: vec![fake_key(2)],
            new_threshold: 2,
            proposal_epoch: 7,
        };
        let c = apply_replace_asset(&mut token, &delta).unwrap();
        assert_eq!(c.bytecode, b"new".to_vec());
        assert_eq!(c.owners, vec![fake_key(2)]);
        assert_eq!(c.threshold, 2);
        // name / version / storage are preserved.
        assert_eq!(c.name, "c");
        assert_eq!(c.version, "1");
        assert_eq!(c.storage.get(&b"k".to_vec()), Some(&b"v".to_vec()));
    }
}
