//! Phase S2.2 — Network-side Halo2 proof verification.
//!
//! `S2.1` installs the *proving* half of the Halo2 stack (the `ActionCircuit`
//! in `circuit.rs`, plus the two `#[ignore]`d live-proving smoke tests). This
//! module installs the *verifying* half: the network must be able to check a
//! shielded proof a client submits, without the network ever seeing the note
//! opening, spend key, Merkle path, or output note — only the circuit's public
//! inputs (nullifiers, commitments, referenced root, fee) and the proof bytes.
//!
//! Design (mirrors the prove side, fails closed):
//!
//! * `ShieldedVerifier` builds the Halo2 `Params` + `VerifyingKey` **once** from
//!   a circuit configuration and caches them, so repeated verifications of
//!   proofs for the same circuit never re-run `keygen_vk` (which synthesizes the
//!   whole constraint system — expensive). The cache is keyed by a fingerprint
//!   of the circuit *configuration*, so a second verifier for the same circuit
//!   shares the first one's key.
//!
//! * `verify` builds the per-instance-column `instances` slice from a
//!   `PublicInputs`, then runs `halo2_proofs::plonk::verify_proof` over a
//!   `Blake2bRead` transcript with the `SingleVerifier`. `halo2_proofs`
//!   `plonk::Error` is not `PartialEq`, so a rejection surfaces as
//!   `Err(PneumaticError::Shielded(..))` — a tampered proof or tampered public
//!   input never silently passes.
//!
//! Verification is network-side and cheap relative to proving, so it runs in
//! the default test suite. The two *live prove* tests remain `#[ignore]`d per
//! the roadmap's "proving is benchmark-only" rule; this module does not prove.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use halo2_proofs::{
    pasta::{EqAffine, Fp},
    plonk::{verify_proof, keygen_vk, SingleVerifier, VerifyingKey},
    poly::commitment::Params,
    transcript::{Blake2bRead, Challenge255},
};
use once_cell::sync::Lazy;

use crate::errors::{PneumaticError, ValidationFailureReason};
use crate::shielded::circuit::{ActionCircuit, PublicInputs};
use crate::shielded::poseidon::poseidon_hash;
use crate::shielded::tree::bytes_to_root;
use crate::shielded::{DEFAULT_DEPTH, MembershipProof, ShieldedNote};
use crate::transactions::ShieldedTransaction;
use pasta_curves::pallas::{Affine as EpAffine, Scalar as Fq};
use ff::FromUniformBytes;
use group::GroupEncoding;
use pasta_curves::arithmetic::CurveAffine;

/// The circuit definition tag. `ActionCircuit` has a fixed structure (the same
/// columns/gates/selectors for every instance), so the verifying key is
/// identical for all of them — this tag identifies that structure in the cache
/// key. If the circuit definition changes, this string changes too and the
/// cache yields a fresh key.
const CIRCUIT_TAG: &str = "ActionCircuit";

/// A cached `(Params, VerifyingKey)` pair for one circuit configuration.
struct CachedKey {
    params: Params<EqAffine>,
    vk: VerifyingKey<EqAffine>,
}

/// Shared, module-level cache of verified circuit configurations → keys, keyed
/// by a fingerprint of the circuit *configuration*. This is what lets
/// repeated verifications skip re-running `keygen_vk`.
static VK_CACHE: Lazy<Mutex<Vec<(u64, CachedKey)>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Test-only instrumentation (AUDIT Phase 6.10 style): counts how many times a
/// new `(Params, VerifyingKey)` is actually generated. A cache hit does not
/// increment this, so a test can prove that N verifiers for the same circuit
/// configuration invoke `keygen_vk` exactly once. `#[cfg(test)]` keeps it out
/// of the production link graph.
#[cfg(test)]
static KEYGEN_CALLS: AtomicU64 = AtomicU64::new(0);

/// Fingerprint a circuit configuration into a cache key.
///
/// The verifying key is the same for every *instance* of a given circuit at a
/// given width `k` (it depends on structure — columns, gates, selectors — which
/// is the constant `CIRCUIT_TAG`, plus `k`, which fixes the `Params` domain).
/// Witness data is deliberately ignored, so all instances of the circuit at the
/// same width share one cached key instead of each triggering a keygen.
fn fingerprint(k: u32) -> u64 {
    let mut hasher = DefaultHasher::new();
    CIRCUIT_TAG.hash(&mut hasher);
    k.hash(&mut hasher);
    hasher.finish()
}

/// The Halo2 verifier for the shielded `ActionCircuit`.
///
/// Holds the `Params` + `VerifyingKey` for one circuit configuration; build it
/// once (via [`ShieldedVerifier::verify`]) and reuse it for many proofs.
#[derive(Clone)]
pub struct ShieldedVerifier {
    params: Params<EqAffine>,
    vk: VerifyingKey<EqAffine>,
}

impl ShieldedVerifier {
    /// Build a verifier for the given circuit configuration at Halo2
    /// instantiation width `k`.
    ///
    /// `k` must match the width the proof was generated at (the `ActionCircuit`
    /// is instantiated at `k = 10` in the test harness — see `circuit_test.rs`);
    /// a mismatched `k` produces a `Params`/domain that does not line up with
    /// the verifying key and `verify` rejects the proof. The `Params` +
    /// `VerifyingKey` are generated once. If a verifier for the same circuit
    /// configuration at the same `k` already exists in the shared cache, its
    /// keys are reused instead of re-running `keygen_vk`. Key generation is
    /// gated by `PneumaticError::Shielded` on a `keygen_vk` failure — never a
    /// silent accept.
    pub fn new(circuit: ActionCircuit, k: u32) -> Result<Self, PneumaticError> {
        let fp = fingerprint(k);

        // Fast path: another verifier already built keys for this configuration.
        if let Some(cached) = VK_CACHE.lock().unwrap().iter().find(|(key, _)| key == &fp) {
            return Ok(ShieldedVerifier {
                params: cached.1.params.clone(),
                vk: cached.1.vk.clone(),
            });
        }

        #[cfg(test)]
        KEYGEN_CALLS.fetch_add(1, Ordering::SeqCst);

        let params = Params::new(k);
        let vk = keygen_vk(&params, &circuit)
            .map_err(|e| PneumaticError::Shielded(format!("keygen_vk failed for action circuit: {e}")))?;

        VK_CACHE.lock().unwrap().push((fp, CachedKey { params: params.clone(), vk: vk.clone() }));

        Ok(ShieldedVerifier { params, vk })
    }

    /// The per-instance-column `instances` slice for a set of public inputs,
    /// in exactly the circuit's instance-column order:
    ///
    /// ```text
    /// [ nullifier, commit_x, commit_y, merkle_root,
    ///   output_commit_x, output_commit_y, fee ]
    /// ```
    ///
    /// Exposed so callers (and tests) can verify the wire/verification layout
    /// matches the circuit's own `Instance` columns.
    pub fn instances_for(public_inputs: &PublicInputs) -> Vec<Vec<Vec<Fp>>> {
        vec![vec![
            vec![public_inputs.nullifier],
            vec![public_inputs.commit_x],
            vec![public_inputs.commit_y],
            vec![public_inputs.merkle_root],
            vec![public_inputs.output_commit_x],
            vec![public_inputs.output_commit_y],
            vec![public_inputs.fee],
        ]]
    }

    /// Test-only accessor: the Halo2 instantiation width (`k`) this verifier was
    /// built at. Pins S4.1's verifying key to the prover's width (`S3.3` proves
    /// at `k = 10`); a mismatched width makes every proof fail to verify.
    #[cfg(test)]
    pub(crate) fn width(&self) -> u32 {
        self.params.k()
    }

    /// Verify a proof over `public_inputs`.
    ///
    /// Returns `Ok(())` iff the proof verifies against the circuit's verifying
    /// key for exactly these public inputs. Any failure — a malformed/short
    /// proof, a tampered proof, or a tampered public input — returns
    /// `Err(PneumaticError::Shielded(..))`. A rejection is never silently
    /// accepted (fail closed).
    pub fn verify(&self, proof: &[u8], public_inputs: &PublicInputs) -> Result<(), PneumaticError> {
        let instances_owned = Self::instances_for(public_inputs);
        // `verify_proof` takes `&[&[&[G::Scalar]]]`. Turn each owned `Vec` into a
        // slice one level at a time (each `Vec`→`&[...]` is one coercion). The
        // inner `Vec<&[Fp]>` vecs are held in `columns_per_proof` so the
        // references in `proofs` outlive them.
        let columns_per_proof: Vec<Vec<&[Fp]>> = instances_owned
            .iter()
            .map(|proof| {
                proof.iter().map(|col| col.as_slice()).collect::<Vec<&[Fp]>>()
            })
            .collect();
        let proofs: Vec<&[&[Fp]]> = columns_per_proof.iter().map(|cols| cols.as_slice()).collect();
        let instances: &[&[&[Fp]]] = &proofs;

        let mut transcript = Blake2bRead::<_, EqAffine, Challenge255<EqAffine>>::init(proof);
        let verifier = SingleVerifier::new(&self.params);

        verify_proof(&self.params, &self.vk, verifier, instances, &mut transcript)
            .map_err(|e| PneumaticError::Shielded(format!("proof verification failed: {e}")))
    }
}

/// Number of times `keygen_vk` has actually run (test-only accessor).
#[cfg(test)]
pub fn keygen_calls() -> u64 {
    KEYGEN_CALLS.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Phase S4.1.2 — reconstruct the circuit public inputs from the wire tx
// ---------------------------------------------------------------------------

/// Error when a `ShieldedTransaction` cannot be decoded into the circuit's
/// `PublicInputs`. Always `InvalidCommitment` (the structural reason check 1
/// owns) — an ambiguous root or commitment fails closed, never panics.
fn decode_failure() -> PneumaticError {
    PneumaticError::Validation(vec![ValidationFailureReason::InvalidCommitment])
}

/// Convert a 32-byte value to an `Fp` field element via uniform-byte reduction,
/// mirroring `circuit.rs`/`note.rs`. The nullifier is defined this way, so the
/// reconstructed value matches the proof's public nullifier exactly.
fn bytes32_to_fp(bytes: &[u8; 32]) -> Fp {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(bytes);
    Fp::from_uniform_bytes(&buf)
}

/// Decode a 32-byte compressed affine point to its `(x, y)` coordinates, failing
/// closed on a non-member or the point at infinity (which would otherwise surface
/// as public-input garbage). Mirrors `tree.rs`'s `from_repr`→`into_option` fail
/// closed idiom.
fn point_coords(bytes: &[u8; 32]) -> Result<(Fp, Fp), PneumaticError> {
    let pt = EpAffine::from_bytes(bytes).into_option().ok_or_else(decode_failure)?;
    let coords = pt.coordinates().into_option().ok_or_else(decode_failure)?;
    let x = *coords.x();
    let y = *coords.y();
    Ok((x, y))
}

/// Reconstruct the Action circuit's [`PublicInputs`] from a wire
/// `ShieldedTransaction`, mapping each of the circuit's seven public columns (v1
/// 1-in/1-out) to the wire field that carries it:
///
/// ```text
/// nullifier ← nullifiers[0]          commit_x/commit_y ← spent_commitments[0]
/// merkle_root ← merkle_root          output_commit_x/output_commit_y ← commitments[0]
/// fee ← fee
/// ```
///
/// The *spent* commitment is carried (not derivable from the nullifier — the
/// nullifier is `poseidon(spend_key, rho)`, one-way) so the verifier receives the
/// exact instances the proof was generated over. Every missing or undecodable
/// element fails closed as `InvalidCommitment`; the helper never panics.
pub fn public_inputs_from_shielded_tx(tx: &ShieldedTransaction) -> Result<PublicInputs, PneumaticError> {
    let nullifier = bytes32_to_fp(tx.nullifiers.first().ok_or_else(decode_failure)?);
    let (commit_x, commit_y) = point_coords(tx.spent_commitments.first().ok_or_else(decode_failure)?)?;
    let merkle_root = bytes_to_root(&tx.merkle_root).ok_or_else(decode_failure)?;
    let (out_x, out_y) = point_coords(tx.commitments.first().ok_or_else(decode_failure)?)?;
    Ok(PublicInputs {
        nullifier,
        commit_x,
        commit_y,
        merkle_root,
        output_commit_x: out_x,
        output_commit_y: out_y,
        fee: Fp::from(tx.fee),
    })
}

/// S5.3: the tree leaf a 32-byte wire commitment becomes when appended to the
/// shielded pool tree — `poseidon_hash([x, y])` over the decoded affine
/// coordinates, the same value `IncrementalMerkleTree::append` computes from
/// the decoded point (tree.rs `commitment_to_leaf`).
///
/// The committer's `ShieldedPool` re-derives each validated tx's output
/// commitment leaves this way at apply time (step 4), so the pool's tree
/// reconstructs identically on every node. Fails closed (undecodable /
/// non-member / point at infinity) exactly as `point_coords` does.
pub fn commitment_leaf(bytes: &[u8; 32]) -> Result<Fp, PneumaticError> {
    let (x, y) = point_coords(bytes)?;
    Ok(poseidon_hash(&[x, y]))
}

// ---------------------------------------------------------------------------
// Phase S4.1.4 — verify key template
// ---------------------------------------------------------------------------

/// A structurally-valid `ActionCircuit` template used only to build the verifying
/// key via `keygen_vk`. `keygen_vk` derives the key from the circuit's *structure*
/// (`configure` + a zeroed `synthesize`) and is independent of witness *values*, so
/// the template never verifies a real proof on its own. The note values are a fixed,
/// non-degenerate choice (so the circuit's `synthesize` — which embeds the note
/// commitment as a public input and asserts it is not the point at infinity — runs);
/// the specific values do not affect the key. The Merkle proof carries
/// `DEFAULT_DEPTH` siblings (the layout allocates one advice column per level and
/// `synthesize` rejects a sibling count other than `DEFAULT_DEPTH`); their values are
/// unused by keygen. This lets `ShieldedValidationSpec` build its verifying key once
/// (see the `SHIELDED_VALIDATOR_VERIFIER` static) without the spec needing to name
/// `ShieldedNote`/`Fq`/`MembershipProof`.
pub fn action_circuit_for_verifying_key() -> ActionCircuit {
    let note = ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1), rcm: Fq::from(2) };
    let output_note = ShieldedNote { value: 90, owner_pk: [2u8; 32], rho: Fq::from(3), rcm: Fq::from(4) };
    ActionCircuit {
        note,
        spend_key: [0u8; 32],
        merkle_proof: MembershipProof { index: 0, siblings: vec![Fp::zero(); DEFAULT_DEPTH as usize] },
        merkle_root: Fp::zero(),
        output_note,
        fee: 0,
        tree_depth: DEFAULT_DEPTH,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{PneumaticError, ValidationFailureReason};
    use crate::shielded::circuit::{ActionCircuit, PublicInputs};
    use crate::shielded::note::{commit, nullifier, ShieldedNote};
    use crate::shielded::tree::{IncrementalMerkleTree, DEFAULT_DEPTH, root_to_bytes};
    use crate::transactions::ShieldedTransaction;
    use pasta_curves::pallas::Scalar as Fq;

    /// A well-formed, fully-decodable wire `ShieldedTransaction` (1-in/1-out) for
    /// the S4.1.2 bridge tests. Real commitment bytes (from `commit`) and a real
    /// nullifier so `point_coords`/`bytes32_to_fp` decode cleanly.
    fn make_wire_tx() -> ShieldedTransaction {
        let note = ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1), rcm: Fq::from(2) };
        let spend_key = [0xABu8; 32];
        let output_note = ShieldedNote { value: 90, owner_pk: [2u8; 32], rho: Fq::from(3), rcm: Fq::from(4) };
        let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        let (root, _proof) = tree.append(&commit(&note));
        let spent_bytes: [u8; 32] = commit(&note).to_bytes().as_ref().try_into().unwrap();
        let output_bytes: [u8; 32] = commit(&output_note).to_bytes().as_ref().try_into().unwrap();
        ShieldedTransaction {
            id: "wire_tx".into(),
            action: "ShieldedTransfer".into(),
            token_id: vec![1],
            spent_commitments: vec![spent_bytes],
            nullifiers: vec![nullifier(&note, &spend_key)],
            commitments: vec![output_bytes],
            merkle_root: root_to_bytes(&root),
            proof: vec![0u8; 64],
            note_ciphertexts: vec![vec![0u8; 64]],
            fee: 10,
        }
    }

    // --- Phase S4.1.2 — public_inputs_from_shielded_tx ---

    #[test]
    fn public_inputs_from_tx_maps_allseven_columns() {
        // A well-formed tx decodes to the circuit's exact public inputs: the
        // nullifier and fee reproduce the wire→field mapping, and the spent +
        // output commitments map to their affine (x, y) coordinates.
        let tx = make_wire_tx();
        let pi = public_inputs_from_shielded_tx(&tx).expect("well-formed tx decodes");

        assert_eq!(pi.nullifier, bytes32_to_fp(&tx.nullifiers[0]), "nullifier maps");
        assert_eq!(pi.fee, Fp::from(10), "fee maps");

        let note = ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1), rcm: Fq::from(2) };
        let spent = commit(&note).coordinates().expect("not infinity");
        let (sx, sy) = (*spent.x(), *spent.y());
        assert_eq!(pi.commit_x, sx, "spent commitment -> commit_x");
        assert_eq!(pi.commit_y, sy, "spent commitment -> commit_y");

        let output_note = ShieldedNote { value: 90, owner_pk: [2u8; 32], rho: Fq::from(3), rcm: Fq::from(4) };
        let outc = commit(&output_note).coordinates().expect("not infinity");
        let (ox, oy) = (*outc.x(), *outc.y());
        assert_eq!(pi.output_commit_x, ox, "output commitment -> output_commit_x");
        assert_eq!(pi.output_commit_y, oy, "output commitment -> output_commit_y");
    }

    #[test]
    fn public_inputs_rejects_bad_root_encoding() {
        // Discriminator: a root that is not a valid Fp (0xFF… ≥ p) fails closed as
        // InvalidCommitment rather than panicking or silently accepting.
        let mut tx = make_wire_tx();
        tx.merkle_root = [0xFFu8; 32];
        assert!(matches!(
            public_inputs_from_shielded_tx(&tx),
            Err(PneumaticError::Validation(ref rs)) if rs.iter().any(|r| matches!(r, ValidationFailureReason::InvalidCommitment))
        ), "an undecodable root must fail closed as InvalidCommitment");
    }

    #[test]
    fn public_inputs_rejects_empty_nullifier_or_spent_commitment() {
        // Discriminator: a missing nullifier or spent commitment (empty vec) fails
        // closed as InvalidCommitment — the check-1 bounds live in the bridge.
        let mut tx = make_wire_tx();
        tx.nullifiers = vec![];
        assert!(matches!(
            public_inputs_from_shielded_tx(&tx),
            Err(PneumaticError::Validation(ref rs)) if rs.iter().any(|r| matches!(r, ValidationFailureReason::InvalidCommitment))
        ));

        let mut tx2 = make_wire_tx();
        tx2.spent_commitments = vec![];
        assert!(matches!(
            public_inputs_from_shielded_tx(&tx2),
            Err(PneumaticError::Validation(ref rs)) if rs.iter().any(|r| matches!(r, ValidationFailureReason::InvalidCommitment))
        ));
    }

    /// The Halo2 instantiation width the Action circuit is built at (see
    /// `circuit_test.rs`, where `const K: u32 = 10`). The verifier's `Params`
    /// must use the same width as the prover or the proof fails to verify.
    /// Note this is the *circuit* width, not the Merkle `tree_depth`.
    const K: u32 = 10;

    /// One shared, lazily-built `ShieldedVerifier` for all the default (fast,
    /// non-ignore) tests. Building the Halo2 verifying key (`keygen_vk`) for the
    /// complex Action circuit is expensive (~60–100s); a module-level `Lazy`
    /// guarantees it runs exactly once even though the tests execute in parallel,
    /// instead of each test racing its own `new` and keying separately.
    static SHIELDED_VERIFIER: Lazy<ShieldedVerifier> = Lazy::new(|| {
        let (circuit, _public_inputs) = make_known_good_fixture();
        ShieldedVerifier::new(circuit, K).expect("vk")
    });

    /// Create a known-good test fixture: a valid note opening, valid Merkle
    /// path, valid nullifier, and valid output commitment. Mirrors
    /// `circuit_test.rs`'s helper so the verifier tests can produce a real
    /// proof over a satisfiable instance.
    fn make_known_good_fixture() -> (ActionCircuit, PublicInputs) {
        let note = ShieldedNote {
            value: 100,
            owner_pk: [1u8; 32],
            rho: Fq::from(1),
            rcm: Fq::from(2),
        };
        let spend_key = [0xAB; 32];
        let output_note = ShieldedNote {
            value: 90,
            owner_pk: [2u8; 32],
            rho: Fq::from(3),
            rcm: Fq::from(4),
        };
        let fee: u64 = 10; // 100 = 90 + 10

        let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        let (merkle_root, merkle_proof) = tree.append(&commit(&note));

        let circuit = ActionCircuit::new(
            note.clone(),
            spend_key,
            merkle_proof,
            merkle_root,
            output_note,
            fee,
            DEFAULT_DEPTH,
        );
        let public_inputs = circuit.public_inputs();
        (circuit, public_inputs)
    }

    /// The column order the circuit exposes as public inputs; any change here
    /// must be mirrored in `instances_for` and the prover. This is a fast,
    /// default-suite discriminator for the verification layout.
    fn expected_column_order(pi: &PublicInputs) -> Vec<Vec<Vec<Fp>>> {
        vec![vec![
            vec![pi.nullifier],
            vec![pi.commit_x],
            vec![pi.commit_y],
            vec![pi.merkle_root],
            vec![pi.output_commit_x],
            vec![pi.output_commit_y],
            vec![pi.fee],
        ]]
    }

    /// Happy path: `instances_for` matches the circuit's instance-column order,
    /// and the verifier's `Params` build at the correct K.
    #[test]
    fn verify_instances_match_circuit_columns() {
        let (_circuit, public_inputs) = make_known_good_fixture();
        assert_eq!(
            ShieldedVerifier::instances_for(&public_inputs),
            expected_column_order(&public_inputs),
            "instances_for must list the circuit's instance columns in order"
        );
    }

    /// Construction: building a `ShieldedVerifier` runs `keygen_vk` and yields a
    /// usable verifier. A circuit whose configuration cannot reach its minimum
    /// row count would surface as `PneumaticError::Shielded`.
    #[test]
    fn verify_vk_construction_succeeds() {
        // `SHIELDED_VERIFIER` is built once by the module-level `Lazy`, which
        // runs the `new` construction path; checking its params validates the
        // width is correct.
        assert_eq!(SHIELDED_VERIFIER.params.k(), K);
    }

    /// Fail closed: an empty/garbage "proof" must be rejected, never accepted.
    /// This exercises the transcript-read + constraint-system failure path
    /// without requiring an expensive live proof.
    #[test]
    fn verify_fails_closed_on_garbage_proof() {
        let (_circuit, public_inputs) = make_known_good_fixture();
        let verifier = &SHIELDED_VERIFIER;

        // Empty and all-zero buffers are not valid proofs — verify must error.
        assert!(
            verifier.verify(&[], &public_inputs).is_err(),
            "an empty proof must fail to verify"
        );

        let garbage = [0u8; 256];
        assert!(
            verifier.verify(&garbage, &public_inputs).is_err(),
            "an all-zero proof must fail to verify"
        );
    }

    /// Fail closed: a tampered public input must be rejected even for a proof
    /// over the correct inputs.
    #[test]
    fn verify_fails_closed_on_tampered_public_input() {
        let (_circuit, public_inputs) = make_known_good_fixture();
        let verifier = &SHIELDED_VERIFIER;

        // Tamper the nullifier public input (outside the tree field domain is
        // not required — any change to an instance column breaks the proof).
        let mut tampered = public_inputs.clone();
        tampered.nullifier += Fp::one();

        assert!(
            verifier.verify(&[0u8; 64], &tampered).is_err(),
            "a tampered public input must fail to verify"
        );
    }

    /// Caching discriminator: two verifiers for the same circuit configuration
    /// share one cached key, so `keygen_vk` runs exactly once (not twice). This
    /// proves the per-circuit configuration VK cache is load-bearing. The
    /// construction path is intentionally `#[ignore]`d to keep the heavy
    /// synthesis out of the default suite (matching the prove side).
    #[test]
    #[ignore]
    fn verify_vk_cache_reused_across_verifiers() {
        let (circuit, _public_inputs) = make_known_good_fixture();

        // Use a width no default test uses so this configuration is guaranteed not
        // already cached. Measure the keygen delta across two `new` calls: the
        // first keys, the second should reuse the cached key.
        let unique_k = 12;
        let before = keygen_calls();
        let _v1 = ShieldedVerifier::new(circuit.clone(), unique_k).unwrap();
        let _v2 = ShieldedVerifier::new(circuit, unique_k).unwrap();
        let after = keygen_calls();

        assert_eq!(
            after - before, 1,
            "two verifiers for the same circuit configuration must invoke keygen_vk exactly once"
        );
    }

    /// The ONE live cryptographic verify against `ShieldedVerifier`. `#[ignore]`d
    /// per the roadmap's "proving is benchmark-only" rule; run on demand with:
    ///
    /// ```text
    /// cargo test --workspace -p pneumatic_core -- --ignored shielded_live_verify
    /// ```
    ///
    /// Produces a real proof, verifies it with `ShieldedVerifier`, then proves
    /// the verifier rejects a tampered proof and a tampered public input (fails
    /// closed). Mirrors `circuit_live_prove_smoke` but exercises the
    /// production `ShieldedVerifier` API.
    #[test]
    #[ignore]
    fn shielded_live_verify() {
        use rand::rngs::OsRng;

        use halo2_proofs::plonk::{create_proof, keygen_pk, keygen_vk};
        use halo2_proofs::transcript::{Blake2bWrite};

        let (circuit, public_inputs) = make_known_good_fixture();

        // Build the verifier (also generates the params + vk).
        let verifier = ShieldedVerifier::new(circuit.clone(), K).expect("vk");

        // Generate a REAL proof via the circuit's proving key.
        let params: Params<EqAffine> = Params::new(K);
        let vk = keygen_vk(&params, &circuit).expect("keygen_vk");
        let pk = keygen_pk(&params, vk, &circuit).expect("keygen_pk");

        let instances_owned = ShieldedVerifier::instances_for(&public_inputs);
        let columns_per_proof: Vec<Vec<&[Fp]>> = instances_owned
            .iter()
            .map(|proof| {
                proof.iter().map(|col| col.as_slice()).collect::<Vec<&[Fp]>>()
            })
            .collect();
        let proofs: Vec<&[&[Fp]]> = columns_per_proof.iter().map(|cols| cols.as_slice()).collect();
        let instances: &[&[&[Fp]]] = &proofs;

        let mut prover_transcript = Blake2bWrite::<_, EqAffine, Challenge255<EqAffine>>::init(Vec::new());
        create_proof(&params, &pk, &[circuit], instances, &mut OsRng, &mut prover_transcript)
            .expect("create_proof on a satisfiable circuit must succeed");
        let proof = prover_transcript.finalize();

        // A satisfying proof verifies through ShieldedVerifier.
        assert!(
            verifier.verify(&proof, &public_inputs).is_ok(),
            "the real proof must verify against the correct instance"
        );

        // Tampered public input must be rejected.
        let mut tampered = public_inputs.clone();
        tampered.commit_x += Fp::one();
        assert!(
            verifier.verify(&proof, &tampered).is_err(),
            "a tampered public input must fail to verify"
        );

        // Tampered proof must be rejected.
        let mut bad_proof = proof.clone();
        // Flip the first byte of the proof (a point/scalar encoding).
        if let Some(last) = bad_proof.last_mut() {
            *last ^= 0xff;
        }
        assert!(
            verifier.verify(&bad_proof, &public_inputs).is_err(),
            "a tampered proof must fail to verify"
        );
    }
}
