//! Phase S1.1 — Halo2 proving/verifier stack (Tier 1 shielded value transfer).
//!
//! This module installs the SNARK proving + verifying stack (`halo2_proofs`, a
//! PLONK implementation with **no trusted setup**) and proves it links, builds,
//! and produces a verifiable proof over the pasta `Fp` field — inside a workspace
//! that already compiles the classical Ed25519 / X25519 / AES-256-GCM / PQC tree.
//!
//! The halo2-dependent code lives behind `#[cfg(test)]` because `halo2_proofs`
//! (and the `ff` `Field` trait) are kept as **dev-dependencies** on purpose: the
//! non-test graph (`cargo check --workspace`, `cargo build`) stays clean and the
//! halo2 curve crates never enter the production link graph. They will be
//! promoted to `[dependencies]` when the S2.2 network verifier and the S2.1
//! circuit move halo2 into non-test code (see the shielded plan).
//!
//! Two gated tests live in the `#[cfg(test)]` submodule below. Per the roadmap's
//! "benchmark-only live proving" rule (Part 5), the default `cargo test` run does
//! **no** real cryptographic proof — only setup + local satisfiability:
//!
//! * `shielded_setup_and_mock_verify` — **counted** in the default run. Builds
//!   `Params`, generates the proving/verifying keys, and checks circuit
//!   satisfiability with `MockProver` (a local witness checker, *not* a crypto
//!   proof). A satisfying witness verifies; a tampered public input is rejected.
//!
//! * `shielded_live_prove_smoke` — **`#[ignore]`d**, the *only* live
//!   cryptographic prove/verify in the workspace. Run on demand with
//!   `cargo test --workspace -- --ignored shielded_live_prove_smoke`. It produces
//!   a real proof, verifies a satisfying witness, then proves the verifier
//!   rejects a tampered instance (fails closed).
//!
//! Both tests are load-bearing discriminators: reverting the halo2 integration
//! makes them fail to compile.
//!
//! See `pneumatic-shielded-implementation-plan.md`.

//! Phase S1.2 — off-circuit Poseidon1 reference hash over the Pallas base
//! field `Fp` (`poseidon::poseidon_hash`, `poseidon::PoseidonHasher`). Kept as a
//! production dependency (`pasta_curves`, `ff`) rather than a dev-dependency:
//! the S3 prover crate calls the hash on the wire path, so it must be live in
//! non-test builds. It does **not** implement the SHA-256 `HashProvider` trait
//! (`crypto.rs:645`) — its I/O is `&[Fp]`, not `&[u8]`.

mod note;
pub use note::{
    commit, decrypt_note, encrypt_note, encrypt_note_to_two, nullifier, NotePlaintext, ShieldedNote,
};

mod poseidon;
pub use poseidon::{poseidon_hash, PoseidonHashProvider, PoseidonHasher};

mod tree;
pub use tree::{
    bytes_to_root, root_to_bytes, IncrementalMerkleTree, MembershipProof, TreeState, DEFAULT_DEPTH,
};

// Phase S4.3 — bounded committed-root history: the concrete `MerkleRootHistory`
// (validation.rs:413) that check 3's `is_root_fresh` reads. The trait impl
// lands in S4.3.3 alongside the concrete-type discriminator re-run.
mod roots;
pub use roots::MerkleRootState;

// Phase S5.1 — the read-only shielded-pool seam the shielded roles validate
// against (S4.1's promise at validation.rs:418-421). S5.4 swaps the one
// construction site for the real `Arc<ShieldedPool>`; the roles never change.
mod pool_view;
pub use pool_view::{ShieldedPoolView, SimpleShieldedPoolView};

mod circuit;
pub use circuit::{ActionCircuit, PublicInputs};

// Phase S2.2 — Halo2 proof verification for the Action circuit.
//
// `circuit.rs` provides the proving side (the `ActionCircuit`) and the two
// `#[ignore]`d live-proving smoke tests; `verify.rs` provides the network-side
// verifying half so the network can check a shielded proof a client submits
// using only the circuit's public inputs + the proof bytes — never the note
// opening, spend key, Merkle path, or output note. See `ShieldedVerifier`.

mod verify;
pub use verify::{
    action_circuit_for_verifying_key, public_inputs_from_shielded_tx, ShieldedVerifier,
};

#[cfg(test)]
mod circuit_test;

#[cfg(test)]
mod tests {
    use halo2_proofs::{
        circuit::{Cell, Layouter, Region, SimpleFloorPlanner, Value},
        dev::MockProver,
        pasta::{EqAffine, Fp},
        plonk::{
            create_proof, keygen_pk, keygen_vk, verify_proof,
            Advice, Column, Circuit, ConstraintSystem, Error, Instance, Selector, SingleVerifier,
        },
        poly::{commitment::Params, Rotation},
        transcript::{Blake2bRead, Blake2bWrite, Challenge255},
    };

    /// Log2 of the number of rows this tiny circuit is instantiated at.
    ///
    /// `K = 4` gives a 16-row domain — comfortably larger than the circuit's
    /// minimum row count, and the value the canonical halo2 example uses.
    const K: u32 = 4;

    /// Configuration for the one-gate multiplication circuit: two advice columns
    /// (`a`, `b`) and a public instance column (`out`), with a single `s_mul`
    /// selector enforcing `a * b = out` on row 0.
    #[derive(Clone)]
    struct MulConfig {
        a: Column<Advice>,
        b: Column<Advice>,
        out: Column<Instance>,
        s_mul: Selector,
    }

    /// A circuit enforcing `a * b = out` — the canonical single-gate Halo2
    /// circuit.
    ///
    /// `a` and `b` are private inputs; `out` is exposed as the circuit's single
    /// public input. A satisfying assignment is any `(a, b)` with `out = a * b`.
    #[derive(Clone, Default)]
    struct MulCircuit {
        a: Value<Fp>,
        b: Value<Fp>,
    }

    impl Circuit<Fp> for MulCircuit {
        type Config = MulConfig;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<Fp>) -> Self::Config {
            let a = meta.advice_column();
            let b = meta.advice_column();
            let out = meta.instance_column();

            // Advice and instance columns must be enabled for equality so they
            // participate in the permutation argument — required for `out` to be
            // exposed as a public input.
            meta.enable_equality(a);
            meta.enable_equality(b);
            meta.enable_equality(out);

            let s_mul = meta.selector();
            meta.create_gate("mul", |meta| {
                // Advice column `a` is used twice: on row 0 as the first
                // multiplicand and on row 1 as the product. The gate enforces
                // `a[0] * b[0] = a[1]` on the row where `s_mul` is active.
                let a_cur = meta.query_advice(a, Rotation::cur());
                let b = meta.query_advice(b, Rotation::cur());
                let prod = meta.query_advice(a, Rotation::next());
                let s_mul = meta.query_selector(s_mul);

                // The returned polynomial is constrained to equal zero. When
                // `s_mul = 1` this enforces `a * b = out` (via `a[1]`); when
                // `s_mul = 0` the row is unconstrained.
                vec![s_mul * (a_cur * b - prod)]
            });

            MulConfig { a, b, out, s_mul }
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<Fp>,
        ) -> Result<(), Error> {
            let MulConfig { a, b, out, s_mul } = config;

            // The product witness lives on advice column `a`, row 1 (the row the
            // mul gate reads via `Rotation::next`). Capture its `Cell` id — `Cell`
            // is `Copy` — outside the region closure, since the instance binding
            // must happen on `layouter` *after* the region that owns the closure.
            let mut product_cell: Option<Cell> = None;

            layouter.assign_region(
                || "assign a * b",
                |mut region: Region<'_, Fp>| {
                    region.assign_advice(|| "a", a, 0, || self.a)?;
                    region.assign_advice(|| "b", b, 0, || self.b)?;

                    // Product = a * b. Unknown on the unknown-witness (keygen)
                    // path, always known during proving. Assigned on row 1, the
                    // row the gate's `Rotation::next` reads.
                    let prod_val = self.a.and_then(|a| self.b.map(|b| a * b));
                    let prod_cell = region.assign_advice(|| "out", a, 1, || prod_val)?;
                    product_cell = Some(prod_cell.cell());

                    // Activate the mul gate on row 0 so the constraint
                    // `a[0] * b[0] = a[1]` is enforced by the proving system.
                    s_mul.enable(&mut region, 0)?;

                    Ok(())
                },
            )?;

            // Expose the product witness as the circuit's single public input.
            layouter.constrain_instance(
                product_cell.expect("product cell always assigned"),
                out,
                0,
            )?;

            Ok(())
        }
    }

    /// A satisfying circuit instance with `a = 2`, `b = 3`, so `out = 6`.
    fn satisfying_circuit() -> MulCircuit {
        MulCircuit {
            a: Value::known(Fp::from(2)),
            b: Value::known(Fp::from(3)),
        }
    }

    /// Counted in the default test run. Exercises the key-generation path
    /// (`Params` → proving/verifying keys) and checks satisfiability with
    /// `MockProver` (a local witness checker, not a cryptographic proof).
    #[test]
    fn shielded_setup_and_mock_verify() {
        let circuit = satisfying_circuit();

        // 1. Setup: prove/verify keys derived from the circuit. On failure these
        //    short-circuit with an error rather than being silently accepted.
        let params: Params<EqAffine> = Params::new(K);
        let vk = keygen_vk(&params, &circuit)
            .expect("keygen_vk on a satisfiable circuit must succeed");
        let _pk = keygen_pk(&params, vk, &circuit)
            .expect("keygen_pk on a satisfiable circuit must succeed");

        // 2. Satisfying witness verifies: `out = 2 * 3 = 6`.
        let prover = MockProver::run(K, &circuit, vec![vec![Fp::from(6)]]).unwrap();
        assert_eq!(
            prover.verify(),
            Ok(()),
            "the satisfying witness (out = 6) must verify"
        );

        // 3. Tampered public input is rejected: `7 ≠ 2 * 3`.
        let tampered = MockProver::run(K, &circuit, vec![vec![Fp::from(7)]]).unwrap();
        assert!(
            tampered.verify().is_err(),
            "a tampered instance (7 != 6) must be rejected"
        );
    }

    /// The ONE live cryptographic prove/verify in the workspace. `#[ignore]`d per
    /// the roadmap's "proving is benchmark-only" rule; run on demand with:
    ///
    /// ```text
    /// cargo test --workspace -- --ignored shielded_live_prove_smoke
    /// ```
    ///
    /// Produces a real proof, verifies a satisfying witness, then proves the
    /// verifier rejects a tampered instance (never silently `Ok`).
    #[test]
    #[ignore] // roadmap Part 5 — live proving is benchmark-only; do not run in the default suite.
    fn shielded_live_prove_smoke() {
        use rand::rngs::OsRng;

        let circuit = satisfying_circuit();

        let params: Params<EqAffine> = Params::new(K);
        let vk = keygen_vk(&params, &circuit).expect("keygen_vk");
        let pk = keygen_pk(&params, vk, &circuit).expect("keygen_pk");
        // `keygen_pk` consumes `vk`; the same key is reused for both verify paths.
        let vk = pk.get_vk();

        // Instances are the per-instance-column vectors. There is a single
        // instance column here; `instances` is the correct witness output,
        // `bad` a tampered one.
        let instances: &[&[&[Fp]]] = &[&[&[Fp::from(6)]]];
        let bad_instances: &[&[&[Fp]]] = &[&[&[Fp::from(7)]]];

        // Produce a REAL proof over the pasta `Fp` field via a Blake2b transcript.
        let mut prover_transcript = Blake2bWrite::<_, EqAffine, Challenge255<EqAffine>>::init(Vec::new());
        create_proof(&params, &pk, &[circuit.clone()], instances, &mut OsRng, &mut prover_transcript)
            .expect("create_proof on a satisfiable circuit must succeed");
        let proof = prover_transcript.finalize();

        // (4) A satisfying instance verifies. `SingleVerifier::Output = ()`, so
        // `verify_proof` returns `Result<(), Error>` — assert the Ok path.
        let mut transcript = Blake2bRead::<_, EqAffine, Challenge255<EqAffine>>::init(&proof[..]);
        let verifier = SingleVerifier::new(&params);
        let vres = verify_proof(&params, vk, verifier, instances, &mut transcript);
        // `halo2_proofs::plonk::Error` is not `PartialEq`, so assert on the `Ok`
        // arm rather than comparing results with `assert_eq!`.
        assert!(
            matches!(vres, Ok(())),
            "the real proof must verify against the correct instance"
        );

        // (5) A tampered instance is rejected — never silently `Ok`. Fail closed.
        let mut transcript_bad = Blake2bRead::<_, EqAffine, Challenge255<EqAffine>>::init(&proof[..]);
        let verifier_bad = SingleVerifier::new(&params);
        assert!(
            verify_proof(&params, vk, verifier_bad, bad_instances, &mut transcript_bad).is_err(),
            "the verifier must reject a tampered instance"
        );
    }
}
