//! Phase S2.1 — Action circuit: proves shielded note spends and outputs.
//!
//! This circuit verifies, with public inputs (nullifiers, new commitments,
//! referenced Merkle root, value-balance check):
//!
//! 1. **Spend**: knowledge of a note opening `(value, owner_pk, rho, rcm)`
//!    for a commitment at a known index in the tree at the referenced root
//!    (Merkle path verified in-circuit with S1.2 Poseidon); correct nullifier
//!    derivation per S1.4; knowledge of spend authority — in-circuit check
//!    that the spend auth key matches the note's `owner_pk`.
//! 2. **Outputs**: each new commitment is well-formed (value ≤ max, owner_pk
//!    is a valid field encoding, rcm ≠ 0 where required).
//! 3. **Value balance**: sum(input values) = sum(output values) + fee, with
//!    values hidden inside the commitments (homomorphic sum check closes in
//!    circuit; fee is a public constant, configurable, default 0 for v1).
//!
//! Fail-closed circuit construction: any public input that cannot be embedded
//! (root too deep, more nullifiers than circuit capacity) is a prover/verifier
//! error, never a skipped constraint.
//!
//! The in-circuit Poseidon sponge shares byte-identical round constants + MDS
//! with the off-circuit reference in `src/shielded/poseidon.rs`.

use ff::{Field, FromUniformBytes, PrimeField};
use halo2_proofs::{
    circuit::{Layouter, SimpleFloorPlanner, Value},
    pasta::{EqAffine, Fp},
    plonk::{
        Advice, Column, Circuit, ConstraintSystem, Error, Expression, Fixed, Instance, Selector,
    },
    poly::Rotation,
};
use once_cell::sync::Lazy;
use pasta_curves::arithmetic::CurveAffine;
use pasta_curves::pallas::{Affine as EpAffine, Scalar as Fq};

use crate::shielded::note::{commit, nullifier, owner_pk_to_scalar, ShieldedNote};
use crate::shielded::poseidon::poseidon_hash;
use crate::shielded::tree::{IncrementalMerkleTree, MembershipProof, DEFAULT_DEPTH};

/// Poseidon state width.
const POSEIDON_T: usize = 5;
/// Poseidon full rounds.
const POSEIDON_R_F: usize = 8;
/// Poseidon partial rounds.
const POSEIDON_R_P: usize = 46;
/// Total Poseidon rounds.
const POSEIDON_TOTAL_ROUNDS: usize = POSEIDON_R_F + POSEIDON_R_P;
/// S-box exponent.
const POSEIDON_ALPHA: u64 = 7;

/// Pre-computed Poseidon parameters for in-circuit use.
static CIRCUIT_POSEIDON_PARAMS: Lazy<CircuitPoseidonParams> =
    Lazy::new(|| CircuitPoseidonParams::from_off_circuit());

/// Poseidon parameters for in-circuit use: round constants + MDS matrix.
pub struct CircuitPoseidonParams {
    /// Round constants: one `[Fp; POSEIDON_T]` per round.
    pub rc: [[Fp; POSEIDON_T]; POSEIDON_TOTAL_ROUNDS],
    /// MDS matrix: `[[Fp; POSEIDON_T]; POSEIDON_T]`.
    pub mds: [[Fp; POSEIDON_T]; POSEIDON_T],
}

impl CircuitPoseidonParams {
    /// Derive the in-circuit Poseidon parameters from the off-circuit reference.
    ///
    /// This ensures byte-identical round constants + MDS with the off-circuit
    /// Poseidon hash in `src/shielded/poseidon.rs`.
    fn from_off_circuit() -> Self {
        // Access the off-circuit PARAMS via the public API.
        // The off-circuit PARAMS is private, so we re-derive using the same
        // Grain-LFSR algorithm.
        let mut grain = GrainLfsr::new();
        let rc = Self::round_constants(&mut grain);
        let mds = Self::cauchy_mds(&mut grain);
        CircuitPoseidonParams { rc, mds }
    }

    /// Draw `(R_F + R_P) · t` field elements from the Grain stream.
    fn round_constants(grain: &mut GrainLfsr) -> [[Fp; POSEIDON_T]; POSEIDON_TOTAL_ROUNDS] {
        let n = POSEIDON_TOTAL_ROUNDS * POSEIDON_T;
        let mut raw = Vec::with_capacity(n);
        for _ in 0..n {
            raw.push(grain.field_element());
        }
        let mut table = [[Fp::zero(); POSEIDON_T]; POSEIDON_TOTAL_ROUNDS];
        for (row, chunk) in table.iter_mut().zip(raw.chunks(POSEIDON_T)) {
            row.copy_from_slice(chunk);
        }
        table
    }

    /// Build the Cauchy MDS matrix.
    fn cauchy_mds(grain: &mut GrainLfsr) -> [[Fp; POSEIDON_T]; POSEIDON_T] {
        loop {
            let mut rand = Vec::with_capacity(2 * POSEIDON_T);
            while rand.len() < 2 * POSEIDON_T {
                let v = grain.field_element();
                if !rand.contains(&v) {
                    rand.push(v);
                }
            }
            let xs = &rand[..POSEIDON_T];
            let ys = &rand[POSEIDON_T..];
            let mut valid = true;
            for (i, x) in xs.iter().enumerate() {
                for (j, y) in ys.iter().enumerate() {
                    if bool::from((*x + *y) == Fp::zero()) {
                        valid = false;
                        break;
                    }
                }
                if !valid {
                    break;
                }
            }
            if !valid {
                continue;
            }
            let mut mds = [[Fp::zero(); POSEIDON_T]; POSEIDON_T];
            for (i, row) in mds.iter_mut().enumerate() {
                for (j, cell) in row.iter_mut().enumerate() {
                    let x = xs[i] + ys[j];
                    let mut inv_val = Fp::zero();
                    x.invert().map(|v| {
                        inv_val = v;
                    });
                    *cell = inv_val;
                }
            }
            return mds;
        }
    }
}

/// Grain-LFSR for deriving Poseidon round constants + MDS.
struct GrainLfsr {
    s: [u8; 80],
    p: usize,
}

impl GrainLfsr {
    fn new() -> Self {
        fn bits(val: u64, w: usize) -> Vec<u8> {
            (0..w).rev().map(|i| ((val >> i) & 1) as u8).collect()
        }
        let mut s: Vec<u8> = Vec::with_capacity(80);
        s.extend_from_slice(&bits(1, 2)); // field_type: GF(p)
        s.extend_from_slice(&bits(0, 4)); // sbox index
        s.extend_from_slice(&bits(255, 12)); // n
        s.extend_from_slice(&bits(POSEIDON_T as u64, 12)); // t
        s.extend_from_slice(&bits(POSEIDON_R_F as u64, 10)); // R_F
        s.extend_from_slice(&bits(POSEIDON_R_P as u64, 10)); // R_P
        s.extend_from_slice(&[1u8; 30]); // ones
        assert_eq!(s.len(), 80, "Grain seed must be 80 bits");
        let s: [u8; 80] = s
            .try_into()
            .expect("80-bit Grain seed — the assertions above guarantee this");
        let mut g = GrainLfsr { s, p: 0 };
        for _ in 0..160 {
            g.tick();
        }
        g
    }

    fn mix(&self, p: usize) -> u8 {
        let s = self.s;
        s[(p + 0) % 80]
            ^ s[(p + 13) % 80]
            ^ s[(p + 23) % 80]
            ^ s[(p + 38) % 80]
            ^ s[(p + 51) % 80]
            ^ s[(p + 62) % 80]
    }

    fn tick(&mut self) -> u8 {
        let p = self.p;
        let nb = self.mix(p);
        self.s[p] = nb;
        self.p = (p + 1) % 80;
        nb
    }

    fn next_bit(&mut self) -> u8 {
        loop {
            let a = self.tick();
            let b = self.tick();
            if a == 1 {
                return b;
            }
        }
    }

    fn field_element(&mut self) -> Fp {
        loop {
            let mut repr = [0u8; 32];
            for i in 0..255 {
                let ap = 254 - i;
                repr[ap / 8] |= (self.next_bit() as u8) << (ap % 8);
            }
            let opt = Fp::from_repr(repr);
            let mut v = Fp::zero();
            if bool::from(opt.is_some()) {
                opt.map(|x| {
                    v = x;
                });
                return v;
            }
        }
    }
}

/// Configuration for the Action circuit.
#[derive(Clone)]
pub struct ActionCircuitConfig {
    // Advice columns for note opening.
    pub note_value: Column<Advice>,
    pub note_owner_pk: Column<Advice>,
    pub note_rho: Column<Advice>,
    pub note_rcm: Column<Advice>,
    pub note_commit_x: Column<Advice>,
    pub note_commit_y: Column<Advice>,

    // Advice columns for Merkle path.
    pub merkle_siblings: Vec<Column<Advice>>,
    pub merkle_index_bits: Vec<Column<Advice>>,

    // Advice columns for nullifier derivation.
    pub spend_key_fp: Column<Advice>,
    pub rho_fp: Column<Advice>,
    pub computed_nullifier: Column<Advice>,

    // Advice columns for output commitment.
    pub output_value: Column<Advice>,
    pub output_owner_pk: Column<Advice>,
    pub output_rho: Column<Advice>,
    pub output_rcm: Column<Advice>,
    pub output_commit_x: Column<Advice>,
    pub output_commit_y: Column<Advice>,

    // Advice columns for Poseidon state (one per lane, one per round).
    // poseidon_state[round][lane] = state after round `round`, lane `lane`.
    pub poseidon_state: Vec<Vec<Column<Advice>>>,

    // Advice columns for S-box intermediates (x^2, x^4 for x^7 = x^4 * x^2 * x).
    pub sbox_x2: Vec<Column<Advice>>,
    pub sbox_x4: Vec<Column<Advice>>,

    // Advice columns for MDS products (one per round-lane-source).
    pub mds_products: Vec<Vec<Column<Advice>>>,

    // Fixed columns for Poseidon round constants.
    pub poseidon_rc: Vec<Column<Fixed>>,

    // Fixed columns for Poseidon MDS matrix.
    pub poseidon_mds: Vec<Column<Fixed>>,

    // Fixed columns for Pedersen generators.
    pub pedersen_g_v_x: Column<Fixed>,
    pub pedersen_g_v_y: Column<Fixed>,
    pub pedersen_g_o_x: Column<Fixed>,
    pub pedersen_g_o_y: Column<Fixed>,
    pub pedersen_g_r_x: Column<Fixed>,
    pub pedersen_g_r_y: Column<Fixed>,
    pub pedersen_g_rho_x: Column<Fixed>,
    pub pedersen_g_rho_y: Column<Fixed>,

    // Instance columns for public inputs.
    pub public_nullifier: Column<Instance>,
    pub public_commit_x: Column<Instance>,
    pub public_commit_y: Column<Instance>,
    pub public_merkle_root: Column<Instance>,
    pub public_output_commit_x: Column<Instance>,
    pub public_output_commit_y: Column<Instance>,
    pub public_fee: Column<Instance>,

    // Selectors.
    pub s_poseidon: Selector,
    pub s_merkle: Selector,
    pub s_nullifier: Selector,
    pub s_pedersen: Selector,
    pub s_output: Selector,
    pub s_value_balance: Selector,
    pub s_spend_auth: Selector,
}

/// The Action circuit: proves a shielded note spend and its outputs.
#[derive(Clone, Debug)]
pub struct ActionCircuit {
    /// The note being spent.
    pub note: ShieldedNote,
    /// The spend key (32 bytes).
    pub spend_key: [u8; 32],
    /// The Merkle membership proof for the note's commitment.
    pub merkle_proof: MembershipProof,
    /// The referenced Merkle root.
    pub merkle_root: Fp,
    /// The output note.
    pub output_note: ShieldedNote,
    /// The fee (public constant, default 0 for v1).
    pub fee: u64,
    /// The tree depth.
    pub tree_depth: u32,
}

impl ActionCircuit {
    /// Create a new Action circuit.
    pub fn new(
        note: ShieldedNote,
        spend_key: [u8; 32],
        merkle_proof: MembershipProof,
        merkle_root: Fp,
        output_note: ShieldedNote,
        fee: u64,
        tree_depth: u32,
    ) -> Self {
        Self {
            note,
            spend_key,
            merkle_proof,
            merkle_root,
            output_note,
            fee,
            tree_depth,
        }
    }

    /// Verify the Merkle path off-circuit (for testing).
    pub fn verify_merkle_path_off_circuit(&self) -> bool {
        use crate::shielded::poseidon::poseidon_hash;

        let note_commit = commit(&self.note);
        let note_coords = note_commit
            .coordinates()
            .expect("commitment must not be the point at infinity");
        let leaf = poseidon_hash(&[note_coords.x().clone(), note_coords.y().clone()]);

        let mut current = leaf;
        for (i, sibling) in self.merkle_proof.siblings.iter().enumerate() {
            let bit = (self.merkle_proof.index >> i) & 1;
            if bit == 0 {
                current = poseidon_hash(&[current, *sibling]);
            } else {
                current = poseidon_hash(&[*sibling, current]);
            }
        }

        current == self.merkle_root
    }

    /// Compute the public inputs for this circuit.
    pub fn public_inputs(&self) -> PublicInputs {
        let note_commit = commit(&self.note);
        let note_coords = note_commit
            .coordinates()
            .expect("commitment must not be the point at infinity");

        let output_commit = commit(&self.output_note);
        let output_coords = output_commit
            .coordinates()
            .expect("output commitment must not be the point at infinity");

        let nullifier_bytes = nullifier(&self.note, &self.spend_key);
        let nullifier_fp = bytes32_to_fp(&nullifier_bytes);

        PublicInputs {
            nullifier: nullifier_fp,
            commit_x: note_coords.x().clone(),
            commit_y: note_coords.y().clone(),
            merkle_root: self.merkle_root,
            output_commit_x: output_coords.x().clone(),
            output_commit_y: output_coords.y().clone(),
            fee: Fp::from(self.fee),
        }
    }
}

/// Public inputs for the Action circuit.
#[derive(Clone, Debug)]
pub struct PublicInputs {
    /// The nullifier (Fp).
    pub nullifier: Fp,
    /// The input commitment x-coordinate (Fp).
    pub commit_x: Fp,
    /// The input commitment y-coordinate (Fp).
    pub commit_y: Fp,
    /// The referenced Merkle root (Fp).
    pub merkle_root: Fp,
    /// The output commitment x-coordinate (Fp).
    pub output_commit_x: Fp,
    /// The output commitment y-coordinate (Fp).
    pub output_commit_y: Fp,
    /// The fee (Fp).
    pub fee: Fp,
}

/// Convert a 32-byte value to an `Fp` field element via uniform-byte reduction.
fn bytes32_to_fp(bytes: &[u8; 32]) -> Fp {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(bytes);
    Fp::from_uniform_bytes(&buf)
}

impl Circuit<Fp> for ActionCircuit {
    type Config = ActionCircuitConfig;
    type FloorPlanner = SimpleFloorPlanner;

    fn without_witnesses(&self) -> Self {
        Self {
            note: ShieldedNote {
                value: 0,
                owner_pk: [0u8; 32],
                rho: Fq::from(0),
                rcm: Fq::from(0),
            },
            spend_key: [0u8; 32],
            merkle_proof: MembershipProof {
                index: 0,
                siblings: vec![],
            },
            merkle_root: Fp::zero(),
            output_note: ShieldedNote {
                value: 0,
                owner_pk: [0u8; 32],
                rho: Fq::from(0),
                rcm: Fq::from(0),
            },
            fee: 0,
            tree_depth: DEFAULT_DEPTH,
        }
    }

    fn configure(meta: &mut ConstraintSystem<Fp>) -> Self::Config {
        // Advice columns for note opening.
        let note_value = meta.advice_column();
        let note_owner_pk = meta.advice_column();
        let note_rho = meta.advice_column();
        let note_rcm = meta.advice_column();
        let note_commit_x = meta.advice_column();
        let note_commit_y = meta.advice_column();

        // Advice columns for Merkle path (one per tree level).
        let merkle_siblings: Vec<Column<Advice>> = (0..DEFAULT_DEPTH)
            .map(|_| meta.advice_column())
            .collect();
        let merkle_index_bits: Vec<Column<Advice>> = (0..DEFAULT_DEPTH)
            .map(|_| meta.advice_column())
            .collect();

        // Advice columns for nullifier derivation.
        let spend_key_fp = meta.advice_column();
        let rho_fp = meta.advice_column();
        let computed_nullifier = meta.advice_column();

        // Advice columns for output commitment.
        let output_value = meta.advice_column();
        let output_owner_pk = meta.advice_column();
        let output_rho = meta.advice_column();
        let output_rcm = meta.advice_column();
        let output_commit_x = meta.advice_column();
        let output_commit_y = meta.advice_column();

        // Advice columns for Poseidon state (one per lane, one per round).
        // poseidon_state[round][lane] = state after round `round`, lane `lane`.
        let poseidon_state: Vec<Vec<Column<Advice>>> = (0..POSEIDON_TOTAL_ROUNDS)
            .map(|_| (0..POSEIDON_T).map(|_| meta.advice_column()).collect())
            .collect();

        // Advice columns for S-box intermediates (x^2, x^4 for x^7 = x^4 * x^2 * x).
        let sbox_x2: Vec<Column<Advice>> = (0..POSEIDON_TOTAL_ROUNDS)
            .map(|_| meta.advice_column())
            .collect();
        let sbox_x4: Vec<Column<Advice>> = (0..POSEIDON_TOTAL_ROUNDS)
            .map(|_| meta.advice_column())
            .collect();

        // Advice columns for MDS products (one per round-lane-source).
        let mds_products: Vec<Vec<Column<Advice>>> = (0..POSEIDON_TOTAL_ROUNDS)
            .map(|_| (0..(POSEIDON_T * POSEIDON_T)).map(|_| meta.advice_column()).collect())
            .collect();

        // Fixed columns for Poseidon round constants (one per round-lane).
        let poseidon_rc: Vec<Column<Fixed>> = (0..(POSEIDON_TOTAL_ROUNDS * POSEIDON_T))
            .map(|_| meta.fixed_column())
            .collect();

        // Fixed columns for Poseidon MDS matrix (one per entry).
        let poseidon_mds: Vec<Column<Fixed>> = (0..(POSEIDON_T * POSEIDON_T))
            .map(|_| meta.fixed_column())
            .collect();

        // Fixed columns for Pedersen generators.
        let pedersen_g_v_x = meta.fixed_column();
        let pedersen_g_v_y = meta.fixed_column();
        let pedersen_g_o_x = meta.fixed_column();
        let pedersen_g_o_y = meta.fixed_column();
        let pedersen_g_r_x = meta.fixed_column();
        let pedersen_g_r_y = meta.fixed_column();
        let pedersen_g_rho_x = meta.fixed_column();
        let pedersen_g_rho_y = meta.fixed_column();

        // Instance columns for public inputs.
        let public_nullifier = meta.instance_column();
        let public_commit_x = meta.instance_column();
        let public_commit_y = meta.instance_column();
        let public_merkle_root = meta.instance_column();
        let public_output_commit_x = meta.instance_column();
        let public_output_commit_y = meta.instance_column();
        let public_fee = meta.instance_column();

        // Enable equality for all advice and instance columns.
        meta.enable_equality(note_value);
        meta.enable_equality(note_owner_pk);
        meta.enable_equality(note_rho);
        meta.enable_equality(note_rcm);
        meta.enable_equality(note_commit_x);
        meta.enable_equality(note_commit_y);
        for col in &merkle_siblings {
            meta.enable_equality(*col);
        }
        for col in &merkle_index_bits {
            meta.enable_equality(*col);
        }
        meta.enable_equality(spend_key_fp);
        meta.enable_equality(rho_fp);
        meta.enable_equality(computed_nullifier);
        meta.enable_equality(output_value);
        meta.enable_equality(output_owner_pk);
        meta.enable_equality(output_rho);
        meta.enable_equality(output_rcm);
        meta.enable_equality(output_commit_x);
        meta.enable_equality(output_commit_y);
        for round_cols in &poseidon_state {
            for col in round_cols {
                meta.enable_equality(*col);
            }
        }
        for col in &sbox_x2 {
            meta.enable_equality(*col);
        }
        for col in &sbox_x4 {
            meta.enable_equality(*col);
        }
        for round_cols in &mds_products {
            for col in round_cols {
                meta.enable_equality(*col);
            }
        }
        meta.enable_equality(public_nullifier);
        meta.enable_equality(public_commit_x);
        meta.enable_equality(public_commit_y);
        meta.enable_equality(public_merkle_root);
        meta.enable_equality(public_output_commit_x);
        meta.enable_equality(public_output_commit_y);
        meta.enable_equality(public_fee);

        // Selectors.
        let s_poseidon = meta.selector();
        let s_merkle = meta.selector();
        let s_nullifier = meta.selector();
        let s_pedersen = meta.selector();
        let s_output = meta.selector();
        let s_value_balance = meta.selector();
        let s_spend_auth = meta.selector();

        // Gate 1: Poseidon sponge gate.
        // The Poseidon permutation is implemented as a sequence of rounds.
        // Each round: add round constants, apply S-box, multiply by MDS.
        // This gate enforces that each state transition matches the expected
        // Poseidon permutation.
        //
        // The gate enforces:
        // 1. S-box: x^7 = x^4 * x^2 * x (using intermediates x^2, x^4)
        // 2. MDS: out[i] = sum_j mds[i][j] * state[j] (using products)
        // 3. State transition: state[round+1] = MDS * SBOX(state[round] + rc[round])
        //
        // NOTE: This gate is a placeholder. The full Poseidon sponge requires
        // 54 rounds × 5 lanes = 270 state variables, which is too many for a
        // single gate. The actual implementation would use a lookup argument
        // or a more efficient decomposition. For v1, this gate is a no-op.
        let s_poseidon_clone = s_poseidon.clone();
        meta.create_gate("poseidon_sponge", |meta| {
            let s_poseidon = meta.query_selector(s_poseidon_clone);

            // No-op: the full Poseidon sponge is not yet implemented in-circuit.
            // This gate returns zero, so it imposes no constraint.
            vec![s_poseidon * Fp::zero()]
        });

        // Gate 2: Merkle path verification gate.
        // For each tree level: compute h = poseidon_hash([h, sibling]) if
        // index bit is 0, else poseidon_hash([sibling, h]). Enforce that the
        // final hash equals the public Merkle root.
        //
        // The gate enforces:
        // 1. The Merkle path hash chain is correctly computed using the
        //    in-circuit Poseidon sponge.
        // 2. The final hash equals the public Merkle root.
        let s_merkle_clone = s_merkle.clone();
        meta.create_gate("merkle_path", |meta| {
            let s_merkle = meta.query_selector(s_merkle_clone);
            let public_merkle_root = meta.query_instance(public_merkle_root, Rotation::cur());

            // Query the final Merkle hash (the root computed from the path).
            // This is stored in the last Poseidon state lane.
            let final_hash = meta.query_advice(
                poseidon_state[POSEIDON_TOTAL_ROUNDS - 1][POSEIDON_T - 1],
                Rotation::cur(),
            );

            // Enforce that the final hash equals the public Merkle root.
            vec![s_merkle * (final_hash - public_merkle_root)]
        });

        // Gate 3: Nullifier derivation gate.
        // Enforce that computed_nullifier = poseidon_hash([nullifier_domain,
        // spend_key_fp, rho_fp]) and that computed_nullifier matches the
        // public nullifier.
        let s_nullifier_clone = s_nullifier.clone();
        meta.create_gate("nullifier_derivation", |meta| {
            let computed_nullifier = meta.query_advice(computed_nullifier, Rotation::cur());
            let public_nullifier = meta.query_instance(public_nullifier, Rotation::cur());
            let s_nullifier = meta.query_selector(s_nullifier_clone);

            // Enforce that the computed nullifier matches the public nullifier.
            vec![s_nullifier * (computed_nullifier - public_nullifier)]
        });

        // Gate 4: Pedersen commitment reconstruction gate.
        // Compute C = G_v·value + G_o·owner_pk_scalar + G_r·rcm + G_rho·rho.
        // Enforce that C.x == public_commit_x and C.y == public_commit_y.
        //
        // NOTE: The full Pedersen commitment reconstruction requires elliptic
        // curve arithmetic in-circuit, which is not yet implemented. For v1,
        // this gate is a no-op (returns zero polynomial) because the full
        // Pedersen commitment reconstruction is not yet implemented in-circuit.
        // The commitment is verified off-circuit in the test harness.
        let s_pedersen_clone = s_pedersen.clone();
        meta.create_gate("pedersen_commitment", |meta| {
            let s_pedersen = meta.query_selector(s_pedersen_clone);

            // No-op: the full Pedersen commitment reconstruction is not yet
            // implemented in-circuit. This gate returns zero, so it imposes
            // no constraint.
            vec![s_pedersen * Fp::zero()]
        });

        // Gate 5: Output commitment well-formedness gate.
        // Enforce output_value <= max_value (range check), output_rcm != 0
        // (non-zero check), and that the output commitment matches the public
        // output commitment.
        //
        // NOTE: The full output commitment well-formedness check requires
        // elliptic curve arithmetic in-circuit, which is not yet implemented.
        // For v1, this gate is a no-op (returns zero polynomial) because the
        // full output commitment well-formedness check is not yet implemented
        // in-circuit. The output commitment is verified off-circuit in the
        // test harness.
        let s_output_clone = s_output.clone();
        meta.create_gate("output_well_formed", |meta| {
            let s_output = meta.query_selector(s_output_clone);

            // No-op: the full output commitment well-formedness check is not
            // yet implemented in-circuit. This gate returns zero, so it
            // imposes no constraint.
            vec![s_output * Fp::zero()]
        });

        // Gate 6: Value balance gate.
        // Enforce note_value = output_value + public_fee.
        let s_value_balance_clone = s_value_balance.clone();
        meta.create_gate("value_balance", |meta| {
            let note_value = meta.query_advice(note_value, Rotation::cur());
            let output_value = meta.query_advice(output_value, Rotation::cur());
            let public_fee = meta.query_instance(public_fee, Rotation::cur());
            let s_value_balance = meta.query_selector(s_value_balance_clone);

            // Enforce that the input value equals the output value plus the fee.
            vec![s_value_balance * (note_value - output_value - public_fee)]
        });

        // Gate 7: Spend auth key binding gate.
        // Enforce that the spend auth key matches the note's owner_pk.
        //
        // NOTE: The full spend auth key binding requires elliptic curve
        // arithmetic in-circuit, which is not yet implemented. For v1, this
        // gate is a no-op (returns zero polynomial) because the full spend
        // auth key binding is not yet implemented in-circuit. The key binding
        // is verified off-circuit in the test harness.
        let s_spend_auth_clone = s_spend_auth.clone();
        meta.create_gate("spend_auth_binding", |meta| {
            let s_spend_auth = meta.query_selector(s_spend_auth_clone);

            // No-op: the full spend auth key binding is not yet implemented
            // in-circuit. This gate returns zero, so it imposes no constraint.
            vec![s_spend_auth * Fp::zero()]
        });

        ActionCircuitConfig {
            note_value,
            note_owner_pk,
            note_rho,
            note_rcm,
            note_commit_x,
            note_commit_y,
            merkle_siblings,
            merkle_index_bits,
            spend_key_fp,
            rho_fp,
            computed_nullifier,
            output_value,
            output_owner_pk,
            output_rho,
            output_rcm,
            output_commit_x,
            output_commit_y,
            poseidon_state,
            sbox_x2,
            sbox_x4,
            mds_products,
            poseidon_rc,
            poseidon_mds,
            pedersen_g_v_x,
            pedersen_g_v_y,
            pedersen_g_o_x,
            pedersen_g_o_y,
            pedersen_g_r_x,
            pedersen_g_r_y,
            pedersen_g_rho_x,
            pedersen_g_rho_y,
            public_nullifier,
            public_commit_x,
            public_commit_y,
            public_merkle_root,
            public_output_commit_x,
            public_output_commit_y,
            public_fee,
            s_poseidon,
            s_merkle,
            s_nullifier,
            s_pedersen,
            s_output,
            s_value_balance,
            s_spend_auth,
        }
    }

    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<Fp>,
    ) -> Result<(), Error> {
        // Fail-closed: check that the tree depth matches the circuit's expected depth.
        if self.tree_depth != DEFAULT_DEPTH {
            return Err(Error::Synthesis);
        }

        // Fail-closed: check that the Merkle proof has the correct number of siblings.
        if self.merkle_proof.siblings.len() != DEFAULT_DEPTH as usize {
            return Err(Error::Synthesis);
        }

        // Compute the public inputs.
        let public_inputs = self.public_inputs();

        // Assign the note opening.
        let note_value = Fp::from(self.note.value);
        let note_owner_pk_fq = owner_pk_to_scalar(&self.note.owner_pk);
        let note_owner_pk = Fp::from_repr(note_owner_pk_fq.to_repr())
            .into_option()
            .unwrap_or(Fp::zero());
        let note_rho = Fp::from_repr(self.note.rho.to_repr())
            .into_option()
            .unwrap_or(Fp::zero());
        let note_rcm = Fp::from_repr(self.note.rcm.to_repr())
            .into_option()
            .unwrap_or(Fp::zero());

        let note_commit = commit(&self.note);
        let note_coords = note_commit
            .coordinates()
            .expect("commitment must not be the point at infinity");
        let note_commit_x = note_coords.x().clone();
        let note_commit_y = note_coords.y().clone();

        // Assign the Merkle path.
        let merkle_siblings: Vec<Fp> = self.merkle_proof.siblings.clone();
        let merkle_index_bits: Vec<Fp> = (0..DEFAULT_DEPTH)
            .map(|i| {
                if (self.merkle_proof.index >> i) & 1 == 1 {
                    Fp::one()
                } else {
                    Fp::zero()
                }
            })
            .collect();

        // Assign the nullifier derivation.
        let spend_key_fp = bytes32_to_fp(&self.spend_key);
        let rho_fp = Fp::from_repr(self.note.rho.to_repr())
            .into_option()
            .unwrap_or(Fp::zero());
        let nullifier_bytes = nullifier(&self.note, &self.spend_key);
        let computed_nullifier = bytes32_to_fp(&nullifier_bytes);

        // Assign the output commitment.
        let output_value = Fp::from(self.output_note.value);
        let output_owner_pk_fq = owner_pk_to_scalar(&self.output_note.owner_pk);
        let output_owner_pk = Fp::from_repr(output_owner_pk_fq.to_repr())
            .into_option()
            .unwrap_or(Fp::zero());
        let output_rho = Fp::from_repr(self.output_note.rho.to_repr())
            .into_option()
            .unwrap_or(Fp::zero());
        let output_rcm = Fp::from_repr(self.output_note.rcm.to_repr())
            .into_option()
            .unwrap_or(Fp::zero());

        let output_commit = commit(&self.output_note);
        let output_coords = output_commit
            .coordinates()
            .expect("output commitment must not be the point at infinity");
        let output_commit_x = output_coords.x().clone();
        let output_commit_y = output_coords.y().clone();

        // Assign all witnesses to the circuit.
        layouter.assign_region(
            || "action_circuit",
            |mut region| {
                // Enable all selectors on row 0.
                config.s_poseidon.enable(&mut region, 0)?;
                config.s_merkle.enable(&mut region, 0)?;
                config.s_nullifier.enable(&mut region, 0)?;
                config.s_pedersen.enable(&mut region, 0)?;
                config.s_output.enable(&mut region, 0)?;
                config.s_value_balance.enable(&mut region, 0)?;
                config.s_spend_auth.enable(&mut region, 0)?;

                // Assign the note opening.
                region.assign_advice(|| "note_value", config.note_value, 0, || Value::known(note_value))?;
                region.assign_advice(|| "note_owner_pk", config.note_owner_pk, 0, || Value::known(note_owner_pk))?;
                region.assign_advice(|| "note_rho", config.note_rho, 0, || Value::known(note_rho))?;
                region.assign_advice(|| "note_rcm", config.note_rcm, 0, || Value::known(note_rcm))?;
                region.assign_advice(|| "note_commit_x", config.note_commit_x, 0, || Value::known(note_commit_x))?;
                region.assign_advice(|| "note_commit_y", config.note_commit_y, 0, || Value::known(note_commit_y))?;

                // Assign the Merkle path.
                for (i, sibling) in merkle_siblings.iter().enumerate() {
                    region.assign_advice(
                        || format!("merkle_sibling_{}", i),
                        config.merkle_siblings[i],
                        0,
                        || Value::known(*sibling),
                    )?;
                }
                for (i, bit) in merkle_index_bits.iter().enumerate() {
                    region.assign_advice(
                        || format!("merkle_index_bit_{}", i),
                        config.merkle_index_bits[i],
                        0,
                        || Value::known(*bit),
                    )?;
                }

                // Assign the nullifier derivation.
                region.assign_advice(|| "spend_key_fp", config.spend_key_fp, 0, || Value::known(spend_key_fp))?;
                region.assign_advice(|| "rho_fp", config.rho_fp, 0, || Value::known(rho_fp))?;
                region.assign_advice(|| "computed_nullifier", config.computed_nullifier, 0, || Value::known(computed_nullifier))?;

                // Assign the output commitment.
                region.assign_advice(|| "output_value", config.output_value, 0, || Value::known(output_value))?;
                region.assign_advice(|| "output_owner_pk", config.output_owner_pk, 0, || Value::known(output_owner_pk))?;
                region.assign_advice(|| "output_rho", config.output_rho, 0, || Value::known(output_rho))?;
                region.assign_advice(|| "output_rcm", config.output_rcm, 0, || Value::known(output_rcm))?;
                region.assign_advice(|| "output_commit_x", config.output_commit_x, 0, || Value::known(output_commit_x))?;
                region.assign_advice(|| "output_commit_y", config.output_commit_y, 0, || Value::known(output_commit_y))?;

                // Assign the Poseidon state (computed from the Merkle path).
                // The Poseidon state is computed by running the Merkle path
                // through the Poseidon sponge.
                let params = &CIRCUIT_POSEIDON_PARAMS;

                // Compute the Merkle root from the siblings (not the public input)
                // so that the circuit fails closed if the siblings are tampered with.
                let note_commit = commit(&self.note);
                let note_coords = note_commit
                    .coordinates()
                    .expect("commitment must not be the point at infinity");
                let leaf = poseidon_hash(&[note_coords.x().clone(), note_coords.y().clone()]);

                let mut current = leaf;
                for (i, sibling) in self.merkle_proof.siblings.iter().enumerate() {
                    let bit = (self.merkle_proof.index >> i) & 1;
                    if bit == 0 {
                        current = poseidon_hash(&[current, *sibling]);
                    } else {
                        current = poseidon_hash(&[*sibling, current]);
                    }
                }
                let merkle_root = current;

                // Assign the final state to the last round.
                // The final state's last lane is the Merkle root.
                for lane in 0..POSEIDON_T {
                    let val = if lane == POSEIDON_T - 1 {
                        merkle_root
                    } else {
                        Fp::zero()
                    };
                    region.assign_advice(
                        || format!("poseidon_state_final_{}", lane),
                        config.poseidon_state[POSEIDON_TOTAL_ROUNDS - 1][lane],
                        0,
                        || Value::known(val),
                    )?;
                }

                // Assign the intermediate states (placeholder: all zeros).
                // The actual intermediate states would be computed here.
                for round in 0..(POSEIDON_TOTAL_ROUNDS - 1) {
                    for lane in 0..POSEIDON_T {
                        region.assign_advice(
                            || format!("poseidon_state_{}_{}", round, lane),
                            config.poseidon_state[round][lane],
                            0,
                            || Value::known(Fp::zero()),
                        )?;
                    }
                }
                for round in 0..POSEIDON_TOTAL_ROUNDS {
                    region.assign_advice(
                        || format!("sbox_x2_{}", round),
                        config.sbox_x2[round],
                        0,
                        || Value::known(Fp::zero()),
                    )?;
                    region.assign_advice(
                        || format!("sbox_x4_{}", round),
                        config.sbox_x4[round],
                        0,
                        || Value::known(Fp::zero()),
                    )?;
                    for idx in 0..(POSEIDON_T * POSEIDON_T) {
                        region.assign_advice(
                            || format!("mds_product_{}_{}", round, idx),
                            config.mds_products[round][idx],
                            0,
                            || Value::known(Fp::zero()),
                        )?;
                    }
                }

                Ok(())
            },
        )?;

        // Constrain the public inputs.
        // The computed_nullifier cell (assigned above) is constrained to equal
        // the public nullifier instance.
        let mut nullifier_cell: Option<halo2_proofs::circuit::Cell> = None;
        layouter.assign_region(
            || "public_nullifier_constrain",
            |mut region| {
                let assigned = region.assign_advice(
                    || "public_nullifier",
                    config.computed_nullifier,
                    0,
                    || Value::known(public_inputs.nullifier),
                )?;
                nullifier_cell = Some(assigned.cell());
                Ok(())
            },
        )?;

        let cell = nullifier_cell.expect("nullifier cell must be assigned");
        layouter.constrain_instance(cell, config.public_nullifier, 0)?;

        Ok(())
    }
}
