//! Mesh fragments — a node's signed self-report of its own role directories.
//!
//! # Why self-report
//!
//! The alternative was a central collector node that registers with everyone and
//! queries directories over the control plane. Three reasons that shape loses
//! here:
//!
//! 1. **Registration is stake-gated** (`registration.rs:434`), and the same stake
//!    pool feeds leader selection and the quorum denominator
//!    (`epoch_manager.rs:77` `to_stake_set()` → `quoruming.rs:55` `total_stake()`).
//!    A collector therefore had to *hold stake* to see anything, and a staked key
//!    that never votes permanently inflates the denominator — an observability
//!    tool would have been paid for out of fault tolerance.
//! 2. **Leaves cannot route through other leaves**, so a collector could only
//!    observe nodes it held direct links to: the same interface ceiling that
//!    constrains validators, moved onto the monitoring box.
//! 3. **A single collector is a single point of observability.** With self-report,
//!    the only centralized thing left is the arithmetic, and the arithmetic is a
//!    pure function over signed artifacts that anyone can re-run.
//!
//! A node describing itself needs no registration, no links, and no stake.
//!
//! # What this does and does not prove
//!
//! A fragment proves *what this node believes its directories contain*, under its
//! own signature. It does not prove reachability — which is why
//! [`MeshFragment::delivery_failures`] ships alongside the buckets: "three
//! finalizers listed" and "delivery to two of them has failed 40 times" is the
//! gap that a directory dump alone cannot see, and it is the gap that has caused
//! most of the debugging in this codebase.
//!
//! # Signing
//!
//! Hybrid `(Ed25519 · ML-DSA-44)` over the canonical serialization, same policy
//! as the control plane: both halves must verify (`crypto.rs:126`), so a
//! classical-only signature is rejected. The timestamp is *inside* the signed
//! payload, so a fragment cannot be replayed with a fresh date — shipping a
//! stale file reports as stale, which is what makes the age bound in
//! `mesh-probe` meaningful rather than decorative.
//!
//! # Failure policy
//!
//! Deliberately fail-**soft**: an observability write must never take a validator
//! offline. A failure is logged and the loop continues. The asymmetry with the
//! rest of the codebase (fail-closed everywhere consensus touches) is intentional
//! — see `AGENTS.md`; the blast radius of this path is a missing fragment, which
//! the probe reports as `not-reported`, not as healthy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;

use crate::crypto::AsymCryptoProvider as _;
use crate::errors::PneumaticError;
use crate::node::NodeRegistryType;
use crate::rns::identity::NodeIdentity;

use super::NodeRegistry;

/// How often a node rewrites its fragment.
///
/// Ten seconds: fast enough that a stale cluster is detected within one eviction
/// window (`REGISTRY_MAX_AGE`), slow enough that the signature cost — a hybrid
/// Ed25519 + ML-DSA signature per write — is invisible next to block work.
pub const DEFAULT_FRAGMENT_INTERVAL: Duration = Duration::from_secs(10);

/// Bumped when the payload shape changes. A reader that does not understand the
/// version must refuse the fragment rather than judge a partial view of it.
pub const FRAGMENT_VERSION: u8 = 1;

/// One peer as this node sees it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FragmentPeer {
    /// The peer's transport address, hex. Matches `manifest.json`'s `rhash_hex`.
    pub rhash_hex: String,
    /// True when this node holds a directory binding for the peer — it registered
    /// that node directly and can vouch for it (`NodeRegistryNode::
    /// directory_signature` non-empty). A peer learned from someone else's
    /// directory response is `false`, which is the difference between "I met
    /// this node" and "someone told me about it".
    pub vouched: bool,
    /// Seconds since the last packet seen from this peer, at dump time.
    ///
    /// This is the field that separates a learned-but-dying entry from a live
    /// one: eviction drops a peer at `REGISTRY_MAX_AGE`, so a large age here
    /// means the directory entry is already a fiction. A bucket count alone
    /// cannot tell those two clusters apart.
    pub last_seen_age_secs: u64,
}

/// The reporting node's identity, so a fragment is self-describing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentIdentity {
    /// Our own transport address, hex.
    pub rhash_hex: String,
    /// Our Ed25519 verifying key, hex — the same key `manifest.json` carries, and
    /// the key the signature is checked against.
    pub ed25519_public_key_hex: String,
}

/// A signed self-report of one node's directories.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshFragment {
    pub version: u8,
    /// Unix seconds at signature time. Inside the signed payload on purpose.
    pub written_at_unix: u64,
    pub node: FragmentIdentity,
    /// Bucket name (`"sentinels"`, `"executors"`, …) → peers held there. Key
    /// spelling is shared with the generator's `Role::plural()`; a test in
    /// `testnet-gen` pins the two together.
    pub buckets: BTreeMap<String, Vec<FragmentPeer>>,
    /// `"<bucket>/<rhash_hex>"` → failed fan-out deliveries recorded so far.
    ///
    /// A peer present in `buckets` with a large count here is *known and
    /// unreachable* — the state a directory dump alone cannot distinguish from a
    /// healthy one. Counts are monotonic per process, so they are a trend, not a
    /// rate; the probe compares them across fragments.
    pub delivery_failures: BTreeMap<String, u64>,
    /// Hex hybrid signature over the canonical serialization of this struct with
    /// `signature` cleared.
    pub signature: String,
}

impl MeshFragment {
    /// Read the current directories of `registry` into an unsigned fragment.
    pub fn from_registry(registry: &NodeRegistry, identity: &NodeIdentity) -> Self {
        let mut buckets: BTreeMap<String, Vec<FragmentPeer>> = BTreeMap::new();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        for node_type in NodeRegistryType::iter() {
            let Some(nodes) = registry.get_nodes(&node_type) else {
                continue;
            };
            let mut peers: Vec<FragmentPeer> = nodes
                .iter()
                .map(|entry| FragmentPeer {
                    rhash_hex: hex::encode(&entry.value().rhash),
                    vouched: !entry.value().directory_signature.is_empty(),
                    last_seen_age_secs: entry.value().last_seen.elapsed().as_secs(),
                })
                .collect();
            // Sorted so two dumps of the same state serialize identically. The
            // payload is signed, and a signature over an unstable ordering would
            // fail verification on a node whose DashMap iterated differently.
            peers.sort();
            buckets.insert(bucket_key(&node_type).to_string(), peers);
        }

        let delivery_failures = registry
            .delivery_failures_snapshot()
            .into_iter()
            .map(|(rhash, node_type, count)| {
                (format!("{}/{}", bucket_key(&node_type), hex::encode(rhash)), count)
            })
            .collect();

        let _ = now; // `written_at_unix` is stamped at sign time, not read time.

        MeshFragment {
            version: FRAGMENT_VERSION,
            written_at_unix: 0,
            node: FragmentIdentity {
                rhash_hex: hex::encode(identity.rhash),
                ed25519_public_key_hex: hex::encode(
                    identity.ed25519.public_key().unwrap_or_default(),
                ),
            },
            buckets,
            delivery_failures,
            signature: String::new(),
        }
    }

    /// The bytes the signature covers: this struct with `signature` cleared.
    ///
    /// `serde_json` + `BTreeMap` + a fixed derive field order make this
    /// deterministic for a given fragment; `version` exists so a future change to
    /// that shape is a hard refusal rather than a silent verification failure.
    pub fn signing_payload(&self) -> Result<Vec<u8>, PneumaticError> {
        let mut unsigned = self.clone();
        unsigned.signature = String::new();
        serde_json::to_vec(&unsigned)
            .map_err(|e| PneumaticError::Encoding(format!("fragment encode: {e}")))
    }

    /// Stamp `written_at_unix` with the current wall clock and sign. Signing
    /// before stamping would leave the timestamp unattested, which is the whole
    /// point: staleness has to be provable, not merely asserted by the file.
    pub fn sign_now(&mut self, identity: &NodeIdentity) -> Result<(), PneumaticError> {
        self.written_at_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let payload = self.signing_payload()?;
        let signature = identity.sign_message(&payload)?;
        self.signature = hex::encode(signature);
        Ok(())
    }

    /// Verify the signature against `ed25519_public_key` — the key the *consumer*
    /// expects, not the one the fragment claims. A caller that verified against
    /// the embedded key would accept a fragment that renamed itself.
    pub fn verify(&self, ed25519_public_key: &[u8]) -> bool {
        let Ok(payload) = self.signing_payload() else {
            return false;
        };
        let Ok(signature) = hex::decode(&self.signature) else {
            return false;
        };
        NodeIdentity::verify_message(ed25519_public_key, &payload, &signature)
    }

    /// Total peers across buckets — the "how many links do I actually hold"
    /// number an operator reads first.
    pub fn peer_count(&self) -> usize {
        self.buckets.values().map(Vec::len).sum()
    }

    /// Serialize and land the file in one rename, so a shipping agent can never
    /// pick up a half-written fragment.
    pub fn write_atomically(&self, path: &Path) -> Result<(), PneumaticError> {
        if self.signature.is_empty() {
            return Err(PneumaticError::Encoding(
                "refusing to write an unsigned fragment".to_string(),
            ));
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| PneumaticError::Encoding(format!("fragment encode: {e}")))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    PneumaticError::Io(format!("fragment dir {}: {e}", parent.display()))
                })?;
            }
        }
        // Unique temp name: two reporters sharing a directory must not collide.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes)
            .map_err(|e| PneumaticError::Io(format!("fragment tmp {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| PneumaticError::Io(format!("fragment rename: {e}")))
    }

    /// Write one signed fragment of `registry`'s current state to `path`.
    pub fn dump_to(
        registry: &NodeRegistry,
        identity: &NodeIdentity,
        path: &Path,
    ) -> Result<(), PneumaticError> {
        let mut fragment = MeshFragment::from_registry(registry, identity);
        fragment.sign_now(identity)?;
        fragment.write_atomically(path)
    }

    /// Periodically write fragments until the process exits.
    ///
    /// One thread per node, and the only thing it can lose is a fragment. The
    /// first write happens before returning so a booting node has a fragment on
    /// disk immediately — a probe pointed at a fresh cluster reads "not
    /// reported" otherwise, which looks like the defect this whole path exists to
    /// detect.
    pub fn start_reporter(
        registry: Arc<NodeRegistry>,
        identity: Arc<NodeIdentity>,
        path: PathBuf,
        interval: Duration,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let mut consecutive_failures = 0u32;
            loop {
                match MeshFragment::dump_to(&registry, &identity, &path) {
                    Ok(()) => consecutive_failures = 0,
                    Err(e) => {
                        // Loud but not unbounded: a persistent failure (read-only
                        // volume, wrong path) repeats once per 10th failure
                        // instead of once per tick forever.
                        consecutive_failures += 1;
                        if consecutive_failures == 1 || consecutive_failures % 10 == 0 {
                            eprintln!(
                                "[pneumatic] mesh fragment write failed (#{consecutive_failures}) \
                                 to {}: {e} — this node is invisible to mesh-probe",
                                path.display()
                            );
                        }
                    }
                }
                std::thread::sleep(interval);
            }
        })
    }
}

/// Start fragment reporting if — and only if — the node was configured to.
///
/// One call site per binary, so "a path in `config.json` means reporting is on"
/// is a single rule. The identity is taken from `config` rather than passed in:
/// reporting under an identity other than the node's own would produce fragments
/// that verify under a key `manifest.json` does not list, i.e. a node that is
/// present in the cluster and invisible to the probe.
///
/// The startup line is deliberate. A node whose fragment path is unset, whose
/// volume is read-only, and whose network is down all look the same from the
/// outside — no fragments. Saying where it writes is what makes "why is this node
/// missing from the report" answerable.
pub fn start_if_configured(config: &crate::config::Config, registry: Arc<NodeRegistry>) -> Option<JoinHandle<()>> {
    let path = config.mesh_fragment_path.clone()?;
    let handle = MeshFragment::start_reporter(
        registry,
        Arc::clone(&config.identity),
        path.clone(),
        DEFAULT_FRAGMENT_INTERVAL,
    );
    eprintln!(
        "[pneumatic] mesh fragment reporting to {} every {:?} (verify with mesh-probe)",
        path.display(),
        DEFAULT_FRAGMENT_INTERVAL
    );
    Some(handle)
}

/// Bucket name for a registry type. Lowercase plural, matching the generator's
/// `Role::plural()` so the probe's expectations and a fragment's keys are the
/// same strings; pinned by `testnet-gen/tests/fragments.rs`.
pub fn bucket_key(node_type: &NodeRegistryType) -> &'static str {
    match node_type {
        NodeRegistryType::Committer => "committers",
        NodeRegistryType::Sentinel => "sentinels",
        NodeRegistryType::Executor => "executors",
        NodeRegistryType::Finalizer => "finalizers",
        NodeRegistryType::Archiver => "archivers",
    }
}
