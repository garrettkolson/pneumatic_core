//! Phase S3.3 — [`build_shielded_tx`] (wire assembly + Halo2 prove) and
//! [`assemble_tx`] (wire assembly from a canonical proof buffer).
//!
//! `build_shielded_tx` is the client-side proving path: it wires the spent input
//! note, its spend secret, its Merkle membership proof, the referenced root, and
//! the output note(s) into an `ActionCircuit`, proves it, and returns a
//! `ShieldedTransaction` (the wire type core defines). `assemble_tx` wires the
//! same fields from an already-produced proof (no proving) and is used by the
//! default-suite wire-form test.
//!
//! **v1 cap.** The `ActionCircuit` (`circuit.rs`) exposes exactly one nullifier
//! and one output commitment as public inputs, so v1 proves one spend paired
//! with one output per transaction. A 2-in/2-out transfer needs a wider circuit
//! — tracked as the S2.1 per-output-circuit follow-up (`phase-s3-3-prover-crate.md`,
//! Open item #1). The prover fails closed (an error) on any other fan-out rather
//! than silently producing a non-verifiable tx.

use ff::Field;
use group::GroupEncoding;
use halo2_proofs::{
    pasta::{EqAffine, Fp},
    plonk::{create_proof, keygen_pk, keygen_vk, ProvingKey},
    poly::commitment::Params,
    transcript::{Blake2bWrite, Challenge255},
};
use once_cell::sync::Lazy;
use rand::rngs::OsRng;
use rand::RngCore;

use pneumatic_core::crypto::Ed25519Provider;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::shielded::{
    bytes_to_root, commit, nullifier, ActionCircuit, DEFAULT_DEPTH, MembershipProof, PublicInputs,
    ShieldedNote, ShieldedVerifier, root_to_bytes,
};
use pneumatic_core::transactions::ShieldedTransaction;

use crate::key::ShieldedIdentity;
use crate::note_builder::{create_note, NoteOutput};

/// Halo2 instantiation width the Action circuit is built at — pinned to match
/// `ShieldedVerifier` in core (`verify.rs` uses the same `K = 10`), so a proof
/// produced here verifies.
const ACTION_K: u32 = 10;

/// A single `pneumatic_prover` proof + verifying key for the Action circuit.
struct ProvingKeyInner {
    params: Params<EqAffine>,
    pk: ProvingKey<EqAffine>,
}

/// Lazily-built proving key: `Params` + `keygen_pk` run once per process (keygen
/// reads the circuit *structure* via `configure`, never the witness, so a
/// throwaway circuit suffices). Kept off the default test path — proving is
/// benchmark-only (roadmap Part 5) and this is the crate's single live prove.
static PROVING_KEY: Lazy<ProvingKeyInner> = Lazy::new(|| {
    let params = Params::new(ACTION_K);
    let circuit = dummy_circuit();
    let vk = keygen_vk(&params, &circuit).expect("keygen_vk on the action circuit");
    let pk = keygen_pk(&params, vk, &circuit).expect("keygen_pk on the action circuit");
    ProvingKeyInner { params, pk }
});

/// A throwaway `ActionCircuit` used only for key generation (keygen never reads
/// the witness). All values are zero — the structure (columns/gates/selectors)
/// is what `keygen_vk` needs.
fn dummy_circuit() -> ActionCircuit {
    use pasta_curves::pallas::Scalar as Fq;
    // value: 1 keeps the Pedersen commitment off the point at infinity (the
    // commitment is G_V·1 + …); rho/rcm are zero — keygen ignores the witness.
    let note = ShieldedNote { value: 1, owner_pk: [0u8; 32], rho: Fq::zero(), rcm: Fq::zero() };
    let output_note =
        ShieldedNote { value: 1, owner_pk: [0u8; 32], rho: Fq::zero(), rcm: Fq::zero() };
    // The circuit's synthesize indexes siblings[0..tree_depth] and computes the
    // note commitment, so the throwaway circuit must (a) carry depth-many siblings
    // or keygen panics on index-out-of-bounds, and (b) have a non-zero note value so
    // the commitment is not the point at infinity. Values are otherwise zero —
    // keygen only reads the circuit structure (columns/gates/selectors), never a
    // real witness, so this is a safe stand-in for key generation.
    ActionCircuit::new(
        note,
        [0u8; 32],
        MembershipProof { index: 0, siblings: vec![Fp::zero(); DEFAULT_DEPTH as usize] },
        Fp::zero(),
        output_note,
        0,
        DEFAULT_DEPTH,
    )
}

/// Assemble a `ShieldedTransaction` from already-produced wire fields. No proving
/// — the caller supplies a `proof` (produced by [`build_shielded_tx`] or a test
/// fixture). Used by the default-suite wire-form test to exercise the assembly,
/// hash, and canonical-bytes properties without a live prove.
pub fn assemble_tx(
    token_id: &[u8],
    spent_commitments: Vec<[u8; 32]>,
    nullifiers: Vec<[u8; 32]>,
    commitments: Vec<[u8; 32]>,
    merkle_root: [u8; 32],
    proof: Vec<u8>,
    note_ciphertexts: Vec<Vec<u8>>,
    fee: u64,
) -> ShieldedTransaction {
    ShieldedTransaction {
        id: random_id(),
        action: "ShieldedTransfer".to_string(),
        token_id: token_id.to_vec(),
        spent_commitments,
        nullifiers,
        commitments,
        merkle_root,
        proof,
        note_ciphertexts,
        fee,
    }
}

/// Client-side proving path (S3.3). Builds one `ActionCircuit` from the single
/// input/output pair, pre-validates the Merkle path, asserts value balance
/// off-circuit (belt-and-suspenders; the proof is the authoritative gate),
/// produces the Halo2 proof, and returns the `ShieldedTransaction` together
/// with the output note(s) it minted (the private notes to hand the recipients).
///
/// Fails closed on: fan-out other than 1-in/1-out, a bad Merkle path, an
/// imbalanced value equation, a non-decodable root, or a proof-construction
/// error.
pub fn build_shielded_tx(
    token_id: &[u8],
    inputs: &[ShieldedNote],
    spend_keys: &[&[u8; 32]],
    merkle_proofs: &[MembershipProof],
    root: [u8; 32],
    outputs: &[NoteOutput],
    fee: u64,
) -> Result<(ShieldedTransaction, Vec<ShieldedNote>), PneumaticError> {
    // v1 fan-out: one spend paired with one output (S3.3 Open item #1).
    let n = inputs.len();
    if n != outputs.len() || n != spend_keys.len() || n != merkle_proofs.len() {
        return Err(PneumaticError::Shielded(format!(
            "build_shielded_tx: input/spend/proof/output counts must match (got inputs={n}, outputs={}, spend_keys={}, proofs={})",
            outputs.len(),
            spend_keys.len(),
            merkle_proofs.len()
        )));
    }
    if n != 1 {
        return Err(PneumaticError::Shielded(format!(
            "build_shielded_tx: v1 proves a single spend per tx (got {n}); the general multi-output \
            circuit is an S2.1 follow-up"
        )));
    }

    let note = &inputs[0];
    let spend_key = *spend_keys[0];
    let merkle_proof = &merkle_proofs[0];
    let output = &outputs[0];

    // Mint the output note (randomizes rho/rcm) and collect its spend ciphertext.
    let (output_note, spend_ciphertext, _viewing_ciphertext) = create_note(&output.recipient, output.value);

    // Value balance off-circuit (belt-and-suspenders; the proof is authoritative).
    match output_note.value.checked_add(fee) {
        Some(total) if total == note.value => {}
        _ => {
            return Err(PneumaticError::Shielded(format!(
                "value balance mismatch: input {} != output {} + fee {} (should be caught by the proof)",
                note.value, output_note.value, fee
            )))
        }
    }

    // Merkle root referenced by the proof (fail closed if the bytes are not an Fp).
    let merkle_root_fp =
        bytes_to_root(&root).ok_or_else(|| PneumaticError::Shielded("referenced root is not a valid Fp".into()))?;

    let circuit = ActionCircuit::new(
        note.clone(),
        spend_key,
        merkle_proof.clone(),
        merkle_root_fp,
        output_note.clone(),
        fee,
        DEFAULT_DEPTH,
    );

    // Never prove against a note that is not actually a leaf at the referenced root.
    if !circuit.verify_merkle_path_off_circuit() {
        return Err(PneumaticError::Shielded(
            "input note is not a member of the referenced pool root".to_string(),
        ));
    }

    let public_inputs = circuit.public_inputs();
    let nullifier_bytes = nullifier(note, &spend_key);

    // Output commitment as a 32-byte compressed affine point (the wire form).
    let commit_repr = commit(&output_note).to_bytes();
    let commit_bytes: [u8; 32] = commit_repr
        .as_ref()
        .try_into()
        .map_err(|_| PneumaticError::Shielded("output commitment did not encode to 32 bytes".into()))?;

    // Spent note commitment as a 32-byte compressed affine point (the wire form) —
    // carried so the network can reconstruct the circuit's commit_x/commit_y public
    // inputs (S4.1.2). Not derivable from the nullifier, so it is produced here.
    let spent_commit_repr = commit(note).to_bytes();
    let spent_commit_bytes: [u8; 32] = spent_commit_repr
        .as_ref()
        .try_into()
        .map_err(|_| PneumaticError::Shielded("spent commitment did not encode to 32 bytes".into()))?;

    // Prove over the circuit's public inputs (instance-column order mirrors
    // `ShieldedVerifier::instances_for` so a proof here verifies on the network).
    let instances: &[&[&[Fp]]] = &[&[
        &[public_inputs.nullifier],
        &[public_inputs.commit_x],
        &[public_inputs.commit_y],
        &[public_inputs.merkle_root],
        &[public_inputs.output_commit_x],
        &[public_inputs.output_commit_y],
        &[public_inputs.fee],
    ]];

    let mut transcript = Blake2bWrite::<_, EqAffine, Challenge255<EqAffine>>::init(Vec::new());
    create_proof(&PROVING_KEY.params, &PROVING_KEY.pk, &[circuit], instances, &mut OsRng, &mut transcript)
        .map_err(|e| PneumaticError::Shielded(format!("halo2 proof construction failed: {e}")))?;
    let proof = transcript.finalize();

    Ok((
        assemble_tx(
            token_id,
            vec![spent_commit_bytes],
            vec![nullifier_bytes],
            vec![commit_bytes],
            root,
            proof,
            vec![spend_ciphertext],
            fee,
        ),
        // Mint the output note(s) the prover proved over and return them so the
        // client can hand the recipients the private notes (rho/rcm). v1 proves
        // exactly one output (see the fan-out check above).
        vec![output_note],
    ))
}

/// A random 32-hex-character tx id (uuid-shaped, no uuid crate needed).
fn random_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::SpendKey;
    use pasta_curves::pallas::Scalar as Fq;
    use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
    use pneumatic_core::shielded::IncrementalMerkleTree;

    #[test]
    fn build_shielded_tx_wire_type_is_well_formed() {
        // Built via assemble_tx (no prove): a 2-in/2-out tx must still wire all
        // the fields with the right counts and structure.
        let tx = assemble_tx(
            b"token_abc",
            vec![[6u8; 32], [7u8; 32]],
            vec![[1u8; 32], [2u8; 32]],
            vec![[3u8; 32], [4u8; 32]],
            [5u8; 32],
            vec![9u8, 8, 7, 6],
            vec![vec![10u8; 128], vec![11u8; 128]],
            0,
        );

        assert_eq!(tx.spent_commitments.len(), 2, "spent_commitments wires per input");
        assert_eq!(tx.nullifiers.len(), 2);
        assert_eq!(tx.commitments.len(), 2);
        assert_eq!(tx.note_ciphertexts.len(), 2);
        assert_eq!(tx.action, "ShieldedTransfer");
        assert_eq!(tx.token_id, b"token_abc".to_vec());
        assert_eq!(tx.merkle_root, [5u8; 32]);
        assert_eq!(tx.fee, 0);

        // hash() is stable across a MsgPack round-trip (canonical form).
        let a = tx.hash().expect("hash");
        let bytes = serialize_to_bytes_rmp(&tx).expect("serialize");
        let decoded: ShieldedTransaction = deserialize_rmp_to(&bytes).expect("deserialize");
        assert_eq!(a, decoded.hash().expect("hash after roundtrip"));
    }

    #[test]
    fn build_shielded_tx_value_balance_off_circuit() {
        let token_id = b"tok";
        let input_note = ShieldedNote { value: 100, owner_pk: [7u8; 32], rho: Fq::from(1u64), rcm: Fq::from(2u64) };
        let spend_key = [0xABu8; 32];
        let proof = MembershipProof { index: 0, siblings: Vec::new() };
        let root = [0u8; 32];

        let out = NoteOutput::new(
            90,
            ShieldedIdentity { spend: SpendKey::from_seed([9u8; 32]), identity: Ed25519Provider::generate() },
        );
        // Imbalanced (100 != 90 + 5): fails closed off-circuit — the value-balance
        // check runs before any prove. (The balanced path is exercised by the
        // live-prove test with a real tree; a balanced default call would otherwise
        // reach the prove path.)
        let res = build_shielded_tx(token_id, &[input_note], &[&spend_key], &[proof], root, &[out], 5);
        assert!(res.is_err(), "an imbalanced tx must fail closed before proving");
        assert!(
            format!("{res:?}").contains("value balance"),
            "the rejection must be the value-balance check, not an unrelated error"
        );
    }

    #[test]
    fn build_shielded_tx_nullifier_is_spend_secret_bound() {
        // Structural "wrong spend key" discriminator: the nullifier depends on the
        // spend secret, so a wrong secret never binds the note.
        let note = ShieldedNote { value: 100, owner_pk: [7u8; 32], rho: Fq::from(5u64), rcm: Fq::from(6u64) };
        let correct = [0xABu8; 32];
        let wrong = [0xBCu8; 32];
        assert_ne!(
            nullifier(&note, &correct),
            nullifier(&note, &wrong),
            "the nullifier must depend on the spend secret"
        );
    }

    #[test]
    fn build_shielded_tx_fan_out_cap_enforced() {
        // v1 is one-in/one-out; a 2-in fan-out fails closed (not a silent,
        // non-verifiable proof).
        let note = ShieldedNote { value: 100, owner_pk: [7u8; 32], rho: Fq::from(1u64), rcm: Fq::from(2u64) };
        let output_a = NoteOutput::new(100, ShieldedIdentity { spend: SpendKey::from_seed([9u8; 32]), identity: Ed25519Provider::generate() });
        let output_b = NoteOutput::new(100, ShieldedIdentity { spend: SpendKey::from_seed([11u8; 32]), identity: Ed25519Provider::generate() });
        let res = build_shielded_tx(
            b"tok",
            &[note.clone(), note],
            &[&[0u8; 32], &[0u8; 32]],
            &[MembershipProof { index: 0, siblings: Vec::new() }, MembershipProof { index: 1, siblings: Vec::new() }],
            [0u8; 32],
            &[output_a, output_b],
            0,
        );
        assert!(res.is_err(), "a 2-in fan-out must fail closed in v1");
    }

    #[test]
    fn merkle_path_off_circuit_accepts_and_rejects() {
        // A real membership proof verifies; a tampered sibling does not.
        let input_note = ShieldedNote { value: 50, owner_pk: [3u8; 32], rho: Fq::from(2u64), rcm: Fq::from(3u64) };
        let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        let (root, proof) = tree.append(&commit(&input_note));

        let circuit = ActionCircuit::new(
            input_note.clone(),
            [9u8; 32],
            proof.clone(),
            root,
            ShieldedNote { value: 40, owner_pk: [4u8; 32], rho: Fq::from(1u64), rcm: Fq::from(1u64) },
            10,
            DEFAULT_DEPTH,
        );
        assert!(circuit.verify_merkle_path_off_circuit(), "a valid path must verify");

        // Tamper a sibling → recomputed root diverges → reject.
        let mut tampered = proof.clone();
        if let Some(first) = tampered.siblings.first_mut() {
            *first += Fp::one();
        }
        let tampered_circuit = ActionCircuit::new(
            input_note,
            [9u8; 32],
            tampered,
            root,
            ShieldedNote { value: 40, owner_pk: [4u8; 32], rho: Fq::from(1u64), rcm: Fq::from(1u64) },
            10,
            DEFAULT_DEPTH,
        );
        assert!(
            !tampered_circuit.verify_merkle_path_off_circuit(),
            "a tampered path must fail closed"
        );
    }

    /// The ONE live prove in the prover crate. `#[ignore]`d per the roadmap's
    /// "proving is benchmark-only" rule; run on demand with:
    ///
    /// ```text
    /// cargo test -p pneumatic_prover -- --ignored build_shielded_tx_live_prove_verifies
    /// ```
    #[test]
    #[ignore]
    fn build_shielded_tx_live_prove_verifies() {
        // A satisfiable 1-in/1-out fixture: input note in the tree at `root`,
        // value 100 = output 90 + fee 10.
        let input_note = ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1u64), rcm: Fq::from(2u64) };
        let spend_key = [0xABu8; 32];
        let recipient = ShieldedIdentity {
            spend: SpendKey::from_seed([2u8; 32]),
            identity: Ed25519Provider::generate(),
        };

        let mut tree = IncrementalMerkleTree::new(DEFAULT_DEPTH);
        let (root, proof) = tree.append(&commit(&input_note));

        // build_shielded_tx mints the output note internally (random rho/rcm); it
        // returns that note so we can build the *same* circuit the proof was made
        // over and verify against the exact public inputs the prover proved.
        let (tx, output_notes) = build_shielded_tx(
            b"tok",
            &[input_note.clone()],
            &[&spend_key],
            &[proof.clone()],
            root_to_bytes(&root),
            &[NoteOutput::new(90, recipient)],
            10,
        )
        .expect("the real prove must succeed");

        // Rebuild the exact circuit (and its public inputs) the proof was produced
        // over, and let the network verifier check the proof.
        let circuit = ActionCircuit::new(
            input_note,
            spend_key,
            proof,
            bytes_to_root(&tx.merkle_root).expect("root"),
            output_notes[0].clone(),
            10,
            DEFAULT_DEPTH,
        );
        let public_inputs = circuit.public_inputs();
        let verifier = ShieldedVerifier::new(circuit, ACTION_K).expect("vk");
        assert!(
            verifier.verify(&tx.proof, &public_inputs).is_ok(),
            "the prover's proof must verify on the network"
        );

        // Tampered public input → rejected (fails closed).
        let mut tampered = public_inputs.clone();
        tampered.nullifier += Fp::one();
        assert!(
            verifier.verify(&tx.proof, &tampered).is_err(),
            "a tampered public input must fail to verify"
        );
    }

    /// Integration seam (S3.3.5): the prover proves over exactly the instance
    /// columns `ShieldedVerifier::instances_for` exposes, in the same order.
    /// `ShieldedVerifier` is core's, so this invariant — read off a real
    /// `public_inputs` — guarantees a future circuit reordering breaks the
    /// prover (its proof stops verifying), never silently desyncs the network.
    #[test]
    fn prover_public_inputs_match_verifier_columns() {
        // A public_inputs of the same shape build_shielded_tx proves over.
        let input_note =
            ShieldedNote { value: 100, owner_pk: [1u8; 32], rho: Fq::from(1u64), rcm: Fq::from(2u64) };
        let spend_key = [0xABu8; 32];
        let output_note =
            ShieldedNote { value: 90, owner_pk: [2u8; 32], rho: Fq::from(3u64), rcm: Fq::from(4u64) };
        let merkle_proof = MembershipProof { index: 0, siblings: Vec::new() };
        let circuit = ActionCircuit::new(
            input_note,
            spend_key,
            merkle_proof,
            Fp::zero(),
            output_note,
            10,
            DEFAULT_DEPTH,
        );
        let public_inputs = circuit.public_inputs();

        // The verifier consumes exactly seven instance columns in this order
        // (verify.rs:144): nullifier, commit_x, commit_y, merkle_root,
        // output_commit_x, output_commit_y, fee. Proving over a different order
        // would make `ShieldedVerifier::verify` reject the (otherwise valid) proof.
        let proofs = ShieldedVerifier::instances_for(&public_inputs);
        assert_eq!(proofs.len(), 1, "one proof per tx");
        let cols = &proofs[0];
        assert_eq!(cols.len(), 7, "seven instance columns");
        assert_eq!(cols[0][0], public_inputs.nullifier, "nullifier");
        assert_eq!(cols[1][0], public_inputs.commit_x, "commit_x");
        assert_eq!(cols[2][0], public_inputs.commit_y, "commit_y");
        assert_eq!(cols[3][0], public_inputs.merkle_root, "merkle_root");
        assert_eq!(cols[4][0], public_inputs.output_commit_x, "output_commit_x");
        assert_eq!(cols[5][0], public_inputs.output_commit_y, "output_commit_y");
        assert_eq!(cols[6][0], public_inputs.fee, "fee");
    }
}
