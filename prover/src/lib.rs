//! Phase S3.3 — `pneumatic_prover`: the client-side, **non-networked** prover
//! crate for the Pneumatic shielded value transfer (Tier 1).
//!
//! A wallet runs this crate to *produce* a shielded transfer; the network only
//! *verifies* it (roadmap 2.1 — proving is client-side, the Executor stage is
//! skipped entirely for shielded transactions). The crate therefore contains no
//! networking: it constructs and returns a [`pneumatic_core::transactions::
//! ShieldedTransaction`] for the client to submit, exactly the wire type core
//! defines. It never sends a message and never changes the wire.
//!
//! Build order across this phase:
//! * `key` — the `SpendKey` spend/viewing key model (S3.3.1).
//! * `note_builder` — `create_note` (note creation + ciphertexts, S3.3.2).
//! * `build` — `build_shielded_tx` (wire assembly + Halo2 prove, S3.3.3) and
//!   `assemble_tx` (wiring from a canonical proof, used by the default suite).
//! * `scan` — `scan_for_notes` (viewing-key compliance/audit path, S3.3.4).
//!
//! See `phase-s3-3-prover-crate.md` for the full checklist (files / action /
//! verify / done — discriminator + workspace test-count progression).

mod key;
pub use key::{SpendKey, ShieldedIdentity};

mod note_builder;
pub use note_builder::{create_note, NoteOutput};

mod build;
pub use build::{assemble_tx, build_shielded_tx};

mod scan;
pub use scan::scan_for_notes;
