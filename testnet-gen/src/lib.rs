//! `pneumatic_testnet_gen` — generate a bootable multi-node testnet.
//!
//! Library surface so the emitters are testable without running the binary:
//! `tests/generation.rs` feeds a [`emit::GenSpec`] at a temp dir and hands the
//! result to the *real* loaders (`Config::load_spec_from`,
//! `EnvironmentMetadata`, `NodeIdentity::load`), which is the only check that
//! matters for a generator — the output has to parse as the binaries parse it.
//!
//! See `src/main.rs` for the CLI and its rationale.

pub mod emit;
pub mod topology;
