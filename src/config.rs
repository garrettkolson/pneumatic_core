use std::env;
use std::fs;
use std::io::Error;
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::crypto::AsymCryptoProvider;
use crate::encoding;
use crate::environment::{CostModel, EnvironmentMetadata, EnvironmentMetadataSpec};
use crate::rns::config_builder::DEFAULT_UDP_PORT;
use crate::rns::identity::NodeIdentity;
use strum::IntoEnumIterator;
use crate::node::{NodeBootstrapError, NodeRegistryType, NodeType, NodeTypeConfig};

pub trait IsConfiguration {
    fn is_for_testing(&self) -> bool;
}

/// A bootstrap peer: a node we start with a direct link to. The 64-byte
/// RNS public key is given in hex; the rhash (16-byte truncated SHA-256 of
/// the public key) is derived, never configured.
#[derive(Clone, Serialize, Deserialize)]
pub struct BootstrapPeer {
    /// Hex-encoded 64-byte RNS public key of the peer.
    pub public_key: String,
    pub ip: String,
    pub port: u16,
}

#[derive(Clone)]
pub struct Config {
    /// This node's public key (RNS / Ed25519 identity).
    ///
    /// Always derived from the persistent identity (`identity.ed25519`) at
    /// boot — never from `config.json`. A `public_key` in `config.json` is
    /// accepted but intentionally ignored (see `ConfigSpec.public_key`); the
    /// node must sign with its identity's private key, so a config-supplied
    /// public key has no matching key and must not be honored.
    pub public_key: Vec<u8>,
    pub ip_address: IpAddr,
    /// Where this node writes its signed mesh fragment, or `None` to report
    /// nothing. See `node::registry::fragment`.
    pub mesh_fragment_path: Option<PathBuf>,
    /// This node watches the network instead of participating in it.
    ///
    /// What it changes, and nothing else: the node may *apply* a directory response
    /// from a peer it has never registered with, provided the envelope and every
    /// entry still verify and the peer is reachable. A monitor, explorer, or indexer
    /// holds no stake, so it cannot register, so without this it is answered by
    /// peers and then discards every answer.
    ///
    /// It does **not** relax who this node answers (that path is gated by freshness
    /// and reachability, not by this key), and a participant cannot use it: the
    /// relaxation is refused to any node declaring a consensus role, so turning this
    /// on while running Sentinel/Executor/Finalizer/Committer buys nothing. A
    /// validator accepting directory entries from arbitrary reachable peers would
    /// install attacker-chosen keys into its role buckets, and `send_to_all` fans
    /// real pipeline traffic to whatever a bucket holds.
    ///
    /// No serde attribute: `Config` is built by `Config::build` from `ConfigSpec`,
    /// which is the serde surface.
    pub directory_observer: bool,
    pub rest_api_version: usize,
    pub node_type: NodeType,
    pub node_registry_types: Vec<NodeRegistryType>,
    pub main_environment_id: String,
    pub reconciliation_partition_id: String,
    pub environment_metadata: Arc<DashMap<String, EnvironmentMetadata>>,
    pub type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>,
    /// Persistent node identity (RNS keypair + Ed25519 signing key).
    pub identity: Arc<NodeIdentity>,
    /// Transport rhash of this node (truncated hash of its RNS public key).
    pub rhash: [u8; 16],
    /// Peers to link at boot (one UDP interface per peer).
    pub bootstrap_peers: Vec<BootstrapPeer>,
    /// Own listen UDP port for the RNS transport.
    pub rns_port: u16,
    /// Relay/gateway mode: re-announce and forward traffic for transitive
    /// discovery. `false` for leaves.
    pub transport_enabled: bool
}

impl Config {
    const CONFIG_FILE_LOCATION: &'static str = "config.json";
    const ENV_FILE_LOCATION: &'static str = "/env";

    pub fn build() -> Result<Config, NodeBootstrapError> {
        let spec = match Config::load_spec() {
            Ok(result) => result,
            Err(err) => return Err(NodeBootstrapError::from_io_error(err))
        };

        // build up environment metadata
        let environment_metadata = match Config::get_environment_metadata() {
            Ok(result) => result,
            Err(err) => return Err(NodeBootstrapError::from_io_error(err))
        };

        // Select node registry types based on node type.
        // Full nodes participate in all registries; light nodes participate in core registries.
        let node_registry_types = Self::default_node_registry_types(spec.is_full_node);

        // Populate per-type configurations (min/max connections + minimum stake).
        // These values are protocol-level defaults; real chain state and stake data
        // may refine them at runtime.
        //
        // The defaults seed every type at the uniform floor; the environment
        // spec may then override a type's minimum via `CostModel.per_type_min_stake`
        // (Phase 4.4). Env specs without that field (the common case) apply no
        // override and stay at the uniform default. Config-schema, not a wire change.
        let mut type_configs = Self::default_type_configs();
        if let Some(main_env) = environment_metadata.get(&spec.main_env_id) {
            for node_type in NodeRegistryType::iter() {
                if let Some(&per_type) = main_env.cost_model.per_type_min_stake.get(&node_type) {
                    if let Some(mut tc) = type_configs.get_mut(&node_type) {
                        tc.min_stake = per_type;
                    }
                }
            }
        }
        let type_configs = Arc::new(type_configs);

        // Load (or create) the persistent identity keystore. A corrupt
        // keystore is a hard error — silently regenerating would orphan
        // the node's stake under a new identity.
        let identity_path = spec.identity_path.clone().unwrap_or_else(|| "node_identity.json".to_string());
        let identity = Arc::new(match NodeIdentity::load_or_create(Path::new(&identity_path)) {
            Ok(identity) => identity,
            Err(e) => {
                return Err(NodeBootstrapError {
                    message: format!("failed to load node identity from {}: {}", identity_path, e),
                })
            }
        });
        let public_key = identity
            .ed25519
            .public_key()
            .map_err(|e| NodeBootstrapError {
                message: format!("failed to read ed25519 public key: {}", e),
            })?;

        eprintln!(
            "[pneumatic] node identity rhash={:02x?} ed25519={:?} rns_public_key={:?}",
            identity.rhash,
            hex::encode(&public_key),
            hex::encode(identity.rns.get_public_key().unwrap_or([0u8; 64]))
        );

        let rhash = identity.rhash;
        let ip_address = Self::resolve_bind_address(spec.ip_address.as_deref())?;
        // Empty string is "explicitly off", distinct from absent only in that the
        // operator typed it; both disable reporting.
        let mesh_fragment_path = spec
            .mesh_fragment_path
            .as_deref()
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(PathBuf::from);
        Ok(Config {
            public_key,
            ip_address,
            mesh_fragment_path,
            directory_observer: spec.directory_observer,
            rest_api_version: spec.rest_api_version,
            node_type: if spec.is_full_node { NodeType::Full } else { NodeType::Light },
            node_registry_types,
            environment_metadata,
            main_environment_id: spec.main_env_id,
            reconciliation_partition_id: spec.reconciliation_partition_id,
            type_configs,
            identity,
            rhash,
            bootstrap_peers: spec.bootstrap_peers.clone(),
            rns_port: spec.rns_port.unwrap_or(DEFAULT_UDP_PORT),
            transport_enabled: spec.transport_enabled,
        })
    }

    fn load_spec() -> Result<ConfigSpec, Error> {
        Config::load_spec_from(Path::new(&Config::config_file_location()))
    }

    /// Resolve the two hardcoded config paths, honoring per-process overrides.
    ///
    /// `CONFIG_FILE_LOCATION` is CWD-relative and `ENV_FILE_LOCATION` is the
    /// absolute `/env`, which is exactly right inside a container (compose
    /// bind-mounts `./config/env:/env:ro`) and unusable for N nodes on one host:
    /// every process would read the same spec and the same keystore, and `/env`
    /// needs filesystem-root write access on a plain machine. A testnet launcher
    /// gives each node its own directory instead, so both paths become
    /// per-process:
    ///
    /// | Variable | Unset (unchanged behavior) |
    /// |---|---|
    /// | `PNEUMATIC_CONFIG_FILE` | `config.json` in the CWD |
    /// | `PNEUMATIC_ENV_DIR` | `/env` |
    ///
    /// Overrides are pass-through, not validation: a path that does not exist
    /// fails at the same point with the same error as the default path would
    /// (`fs::read` / `fs::read_dir`), so pointing an override at garbage is
    /// loud-at-boot rather than silently defaulted. An empty value is treated as
    /// unset, because `VAR=` in a shell or a compose file is far more likely a
    /// mistake than an intent to read a file named `""`.
    fn config_file_location() -> String {
        Self::resolve_location(env::var("PNEUMATIC_CONFIG_FILE").ok(), Self::CONFIG_FILE_LOCATION)
    }

    fn env_file_location() -> String {
        Self::resolve_location(env::var("PNEUMATIC_ENV_DIR").ok(), Self::ENV_FILE_LOCATION)
    }

    /// Pure half of the two `*_location` resolvers, so the override rules are
    /// testable without mutating process-global environment variables (which
    /// would race with every other test in the binary).
    fn resolve_location(override_value: Option<String>, default: &'static str) -> String {
        match override_value {
            Some(value) if !value.trim().is_empty() => value,
            _ => default.to_string(),
        }
    }

    /// Which address to bind the transport to. The pure half of the `ip_address`
    /// handling, so the fail-closed rule is testable without a keystore on disk.
    ///
    /// Unset binds the unspecified address. A configured value is parsed
    /// strictly: a malformed address stops boot instead of falling back to
    /// "any", because the fallback produces a node that reports itself healthy
    /// while every peer records a delivery failure against it. Whitespace is
    /// trimmed — these files are hand-edited, and a trailing space is a
    /// formatting slip rather than an intent to bind a different host.
    fn resolve_bind_address(configured: Option<&str>) -> Result<IpAddr, NodeBootstrapError> {
        match configured {
            None => Ok(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
            Some(raw) if raw.trim().is_empty() => Ok(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
            Some(raw) => raw.trim().parse::<IpAddr>().map_err(|e| NodeBootstrapError {
                message: format!("config ip_address {raw:?} is not a valid IP address: {e}"),
            }),
        }
    }

    /// Address the RNS transport should bind, as a string for
    /// `RnsNodeConfigBuilder::with_listen_ip`.
    ///
    /// Single source of truth for every binary that starts a transport, and it
    /// exists because there wasn't one: the committer carried this rule locally
    /// and `node-server` applied none, so the composite silently inherited
    /// `RnsNodeConfigBuilder::new()`'s `127.0.0.1` default and could not be
    /// reached from any other host. Binding loopback is invisible on a
    /// single-host mesh, which is why nothing caught it (10/02/2026).
    ///
    /// An unspecified address binds every interface. A configured one binds
    /// only that interface, which is what a multi-homed instance needs.
    /// Unspecified IPv6 resolves to `0.0.0.0`: the transport's own addresses are
    /// IPv4 in this codebase, so widening to `::` here would change wire
    /// behavior rather than just the bind.
    pub fn rns_listen_ip(&self) -> String {
        if self.ip_address.is_unspecified() {
            "0.0.0.0".to_string()
        } else {
            self.ip_address.to_string()
        }
    }

    /// Filesystem half of `load_spec`, with the path injected so the load/parse
    /// behavior is unit-testable without changing the process CWD
    /// (TASKS.md test-gap tail, 10/01/2026).
    fn load_spec_from(path: &Path) -> Result<ConfigSpec, Error> {
        let file_read = &match fs::read(path) {
            Ok(r) => r,
            Err(e) => return Err(e)
        };

        encoding::deserialize_json_to::<ConfigSpec>(file_read)
    }

    fn get_environment_metadata() -> Result<Arc<DashMap<String, EnvironmentMetadata>>, Error> {
        Config::get_environment_metadata_from(Path::new(&Config::env_file_location()))
    }

    /// Directory-scoped half of `get_environment_metadata` with the directory
    /// injected, so spec loading + validation + fail-closed behavior is
    /// unit-testable against a temp dir (TASKS.md test-gap tail, 10/01/2026).
    fn get_environment_metadata_from(dir: &Path) -> Result<Arc<DashMap<String, EnvironmentMetadata>>, Error> {
        let mut env_specs = vec![];
        for file in fs::read_dir(dir)? {
            let file_path_buf = file?.path();
            let file_path = file_path_buf.as_path();
            let env_file_read = &match fs::read(file_path) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Could not load file {:?} as environment spec", file_path);
                    return Err(e);
                }
            };

            if env_file_read.len() > 0 {
                match encoding::deserialize_json_to::<EnvironmentMetadataSpec>(env_file_read) {
                    Ok(r) => env_specs.push(r),
                    Err(e) => {
                        eprintln!("Could not load file {:?} as environment spec", file_path);
                        return Err(e);
                    }
                }
            }
        }
        let mut environment_metadata = DashMap::new();
        for env_spec in env_specs {
            // Phase 5.7 / H6: reject specs whose security-relevant config is
            // outside the valid range (quorum percentages, max_risk, admin tax,
            // gas multipliers, shard count). A bad spec fails the node here —
            // surfaced as an io error so Config::build() fails boot — instead of
            // silently neutering finalization quorum or the risk gate.
            if let Err(validation_error) = env_spec.validate() {
                eprintln!("Could not load environment spec as valid: {validation_error}");
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    validation_error.to_string(),
                ));
            }
            // Phase 6.5: a spec missing a required Token/Slush partition, or
            // listing an unknown validator spec name, now fails load here
            // instead of panicking — surfaced as an io error so Config::build()
            // fails boot cleanly — rather than aborting the process.
            let env_metadata = match EnvironmentMetadata::load_from_spec(env_spec) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("Could not load environment spec as valid: {e}");
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e.to_string(),
                    ));
                }
            };
            environment_metadata.insert(env_metadata.environment_id.clone(), env_metadata);
        }

        Ok(Arc::new(environment_metadata))
    }

    pub fn get_max_node_number(&self, node_type: &NodeRegistryType) -> usize {
        match self.type_configs.get(node_type) {
            Some(node) => node.max,
            None => 0
        }
    }

    pub fn get_min_type_stake(&self, node_type: &NodeRegistryType) -> u64 {
        match self.type_configs.get(node_type) {
            Some(config) => config.min_stake,
            None => Self::default_min_stake()
        }
    }

    /// The protocol-level global minimum stake for the main environment, from
    /// `CostModel.global_min_stake`. Falls back to the cost-model default (10)
    /// when the environment is absent from the registry (e.g. tests). This is
    /// the second floor on top of the per-type minimum — a node must meet
    /// *both* to register or act (see `meets_minimum_stake`, and the
    /// `ActionRouter::check_stake` reference at `action_router.rs:180`).
    pub fn get_global_min_stake(&self) -> u64 {
        self.environment_metadata
            .get(&self.main_environment_id)
            .map(|env| env.cost_model.global_min_stake)
            .unwrap_or_else(CostModel::default_global_min_stake)
    }

    /// Return the node registry types this node participates in.
    ///
    /// Full nodes participate in all five registry types (Committer, Sentinel,
    /// Executor, Finalizer, Archiver) so they can broadcast to and receive
    /// from any peer type. Light nodes only participate in core registries
    /// (Committer, Sentinel, Executor, Finalizer) to reduce network overhead.
    fn default_node_registry_types(is_full_node: bool) -> Vec<NodeRegistryType> {
        if is_full_node {
            NodeRegistryType::iter().collect()
        } else {
            vec![
                NodeRegistryType::Committer,
                NodeRegistryType::Sentinel,
                NodeRegistryType::Executor,
                NodeRegistryType::Finalizer,
            ]
        }
    }

    /// Build per-type configurations with protocol-level defaults.
    ///
    /// Each `NodeTypeConfig` specifies the minimum/maximum number of registered
    /// nodes for that type and the minimum stake required to join that registry.
    /// These values represent the staking protocol's baseline — chain state and
    /// real stake data may adjust them at runtime.
    fn default_type_configs() -> DashMap<NodeRegistryType, NodeTypeConfig> {
        let stake = Self::default_min_stake();
        let configs = DashMap::new();
        for node_type in NodeRegistryType::iter() {
            configs.insert(
                node_type,
                NodeTypeConfig {
                    min: 1,
                    max: 1000,
                    min_stake: stake,
                },
            );
        }
        configs
    }

    fn default_min_stake() -> u64 {
        10
    }

    /// Build a Config for unit tests without reading from disk. Uses an
    /// ephemeral in-memory identity (no keystore file).
    pub fn new_for_testing(
        main_environment_id: String,
        environment_metadata: Arc<DashMap<String, EnvironmentMetadata>>,
        type_configs: Arc<DashMap<NodeRegistryType, NodeTypeConfig>>,
    ) -> Self {
        let identity = NodeIdentity::generate_in_memory();
        let public_key = identity.ed25519.public_key().unwrap_or_default();
        let rhash = identity.rhash;
        Config {
            public_key,
            ip_address: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            // Tests do not report: a unit test writing fragments would land files
            // in the repository working tree.
            mesh_fragment_path: None,
            directory_observer: false,
            rest_api_version: 1,
            node_type: NodeType::Full,
            node_registry_types: vec![],
            main_environment_id,
            reconciliation_partition_id: String::from("default"),
            environment_metadata,
            type_configs,
            identity: Arc::new(identity),
            rhash,
            bootstrap_peers: Vec::new(),
            rns_port: DEFAULT_UDP_PORT,
            transport_enabled: false,
        }
    }
}

/// Whether `stake` meets the *lower* of the two registered floors.
///
/// Registration and action stake gates enforce a conjunction: a key must
/// satisfy both the protocol-level global minimum and the per-type minimum.
/// `meets_minimum_stake` returns `true` only when `stake` is at or above both —
/// mirroring the AND that `ActionRouter::check_stake` enforces at
/// (`action_router.rs:186-196`) and that the `StakeIndex` registration gate
/// (`node/stake_index.rs`) applies on the off-thread hot path. A `stake` below
/// either floor fails. Kept as a free function so it is reachable from any
/// module via `crate::config::meets_minimum_stake` (an inherent method on
/// `Config` would only be reachable via `Config::meets_minimum_stake`).
pub fn meets_minimum_stake(stake: u64, global_min: u64, type_min: u64) -> bool {
    stake >= global_min && stake >= type_min
}

#[derive(Serialize, Deserialize)]
pub struct ConfigSpec {
    /// Accepted but intentionally IGNORED. Honoring a config-supplied public
    /// key is infeasible (the node signs with its identity's private key,
    /// which is not stored here), so the value is dropped and `Config.public_key`
    /// is derived from the keystore identity instead. Kept so that a future
    /// `#[serde(deny_unknown_fields)]` does not turn this into a hard parse
    /// error and break existing config.json files.
    #[serde(default)]
    public_key: Vec<u8>,
    is_full_node: bool,
    rest_api_version: usize,
    environments: Vec<String>,
    main_env_id: String,
    reconciliation_partition_id: String,
    /// Keystore file path (default `node_identity.json`).
    #[serde(default)]
    identity_path: Option<String>,
    /// Peers to link at boot.
    #[serde(default)]
    bootstrap_peers: Vec<BootstrapPeer>,
    /// Own listen UDP port (default 4242).
    #[serde(default)]
    rns_port: Option<u16>,
    /// Address to bind the RNS transport to. Unset binds the unspecified
    /// address, which `Config::rns_listen_ip` turns into `0.0.0.0`.
    ///
    /// This exists for multi-homed hosts: a cloud instance with a second
    /// interface (an admin NIC on a different network) must bind the interface
    /// its peers can actually reach, because a source bound to the wrong
    /// address is unreachable and every peer just records a delivery failure.
    /// A value that does not parse stops boot rather than falling back — the
    /// fallback failure mode is a node that looks healthy and peers with nobody.
    #[serde(default)]
    ip_address: Option<String>,
    /// Path to write this node's signed mesh fragment (its own role directories,
    /// for `mesh-probe` to aggregate). Unset disables reporting.
    ///
    /// Self-reporting rather than a collector node: a collector would have to hold
    /// stake to register at all (`registration.rs` stake gate), and the same stake
    /// feeds the quorum denominator — so monitoring would be paid for in fault
    /// tolerance. It would also only ever see nodes it links to directly.
    ///
    /// Point this at a directory your log/metrics agent already ships. Writes are
    /// atomic (temp + rename), so a half-written fragment is not a thing the
    /// shipper can pick up.
    #[serde(default)]
    mesh_fragment_path: Option<String>,
    /// Absent ⇒ not an observer. See [`Config::directory_observer`].
    #[serde(default)]
    directory_observer: bool,
    /// Relay/gateway mode (default false = leaf).
    #[serde(default)]
    transport_enabled: bool
}

#[cfg(test)]
mod config_tests {
    use std::sync::Arc;

    use dashmap::DashMap;

    use std::net::{IpAddr, Ipv6Addr};

    use crate::config::{Config, ConfigSpec};
    use crate::crypto::AsymCryptoProvider;

    /// Minimal valid `config.json` shape: only the five required fields
    /// (`environments` is required-but-unused — it must be present or the
    /// spec fails to parse for the wrong reason).
    fn base_spec() -> serde_json::Value {
        serde_json::json!({
            "is_full_node": true,
            "rest_api_version": 1,
            "environments": [],
            "main_env_id": "env",
            "reconciliation_partition_id": "default"
        })
    }

    // Phase 6.8 config hygiene: removing the dead required `balance` field
    // means config.json no longer has to carry a meaningless value. This is
    // the true discriminator — re-adding `balance` (no serde default) makes
    // from_value return Err("missing field \"balance\"") → is_ok() fails.
    #[test]
    fn config_spec_parses_without_balance() {
        let result = serde_json::from_value::<ConfigSpec>(base_spec());
        assert!(result.is_ok(), "spec must parse without a balance field");
    }

    // Removing the field must not add strictness: a spec that still carries
    // `balance` keeps parsing (serde ignores it).
    #[test]
    fn config_spec_still_accepts_balance_field() {
        let mut spec = base_spec();
        spec["balance"] = serde_json::json!(42u64);
        let result = serde_json::from_value::<ConfigSpec>(spec);
        assert!(result.is_ok(), "spec must still parse with a balance field");
    }

    // The (kept, documented) `public_key` field is tolerated and ignored —
    // proves no `#[serde(deny_unknown_fields)]` was introduced.
    #[test]
    fn config_spec_public_key_field_is_tolerated() {
        let mut spec = base_spec();
        spec["public_key"] = serde_json::json!([1u8, 2, 3]);
        let result = serde_json::from_value::<ConfigSpec>(spec);
        assert!(result.is_ok(), "spec must parse with a public_key field");
    }

    // `Config.public_key` is identity-authoritative, never config-derived
    // (honoring a config public_key would break message auth). Reverting the
    // assignment to anything other than the identity flips this assertion.
    #[test]
    fn config_public_key_is_identity_authoritative() {
        let config = Config::new_for_testing(
            "env".into(),
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
        );
        let identity_key = config
            .identity
            .ed25519
            .public_key()
            .expect("in-memory identity yields a real key");
        assert_eq!(config.public_key, identity_key);
    }

    // -----------------------------------------------------------------------
    // Load/parse tests (TASKS.md test-gap tail, 10/01/2026): exercise the
    // path-injected seams — the same file handling `Config::build()` performs
    // against `config.json` and `/env`, but against temp dirs.
    // -----------------------------------------------------------------------

    /// The real deploy environment spec (also parse-validated by
    /// `tests/deploy_examples.rs`), with `log_file` redirected into the test's
    /// temp dir so `load_from_spec` can build its FileLogger anywhere.
    fn valid_env_spec_json(log_path: &str) -> String {
        let mut spec: serde_json::Value =
            serde_json::from_str(include_str!("../deploy/config/env/env.json")).unwrap();
        spec["log_file"] = serde_json::json!(log_path);
        spec.to_string()
    }

    #[test]
    fn load_spec_from_reads_valid_minimal_file_and_applies_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, base_spec().to_string()).unwrap();

        let spec = Config::load_spec_from(&path).expect("valid config.json must parse");
        assert_eq!(spec.main_env_id, "env");
        assert_eq!(spec.rest_api_version, 1);
        assert!(spec.is_full_node);
        assert_eq!(spec.reconciliation_partition_id, "default");
        // Optional fields default exactly as Config::build() expects:
        assert!(spec.identity_path.is_none(), "absent identity_path => keystore default");
        assert!(spec.bootstrap_peers.is_empty());
        assert!(spec.rns_port.is_none(), "absent rns_port => DEFAULT_UDP_PORT at build");
        assert!(!spec.transport_enabled, "leaf by default");
    }

    #[test]
    fn load_spec_from_parses_full_shape_with_optional_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut full = base_spec();
        full["identity_path"] = serde_json::json!("keys/id.json");
        full["rns_port"] = serde_json::json!(4343u64);
        full["transport_enabled"] = serde_json::json!(true);
        full["bootstrap_peers"] = serde_json::json!([{
            "public_key": "ab".repeat(64), // 128 hex chars = 64-byte key
            "ip": "127.0.0.1",
            "port": 4242u64,
        }]);
        std::fs::write(&path, full.to_string()).unwrap();

        let spec = Config::load_spec_from(&path).expect("full-shape spec must parse");
        assert_eq!(spec.identity_path.as_deref(), Some("keys/id.json"));
        assert_eq!(spec.rns_port, Some(4343));
        assert!(spec.transport_enabled);
        assert_eq!(spec.bootstrap_peers.len(), 1);
        assert_eq!(spec.bootstrap_peers[0].port, 4242);
    }

    #[test]
    fn load_spec_from_missing_file_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = match Config::load_spec_from(&dir.path().join("nope.json")) {
            Err(e) => e,
            Ok(_) => panic!("missing file must fail"),
        };
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn load_spec_from_unparseable_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"{ not json at all").unwrap();
        assert!(Config::load_spec_from(&path).is_err(), "corrupt config.json must fail boot");
    }

    #[test]
    fn load_spec_from_missing_required_field_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut spec = base_spec();
        spec.as_object_mut().unwrap().remove("main_env_id");
        std::fs::write(&path, spec.to_string()).unwrap();
        assert!(
            Config::load_spec_from(&path).is_err(),
            "a spec missing a required field must not build"
        );
    }

    /// Without the override set, both paths resolve exactly as they always did —
    /// containers keep mounting `/env` and binaries keep reading `config.json`
    /// from the CWD.
    #[test]
    fn config_path_override_defaults_are_unchanged() {
        assert_eq!(
            Config::resolve_location(None, Config::CONFIG_FILE_LOCATION),
            "config.json"
        );
        assert_eq!(
            Config::resolve_location(None, Config::ENV_FILE_LOCATION),
            "/env"
        );
        assert_eq!(
            Config::resolve_location(Some("/tmp/node-3/config.json".into()), Config::CONFIG_FILE_LOCATION),
            "/tmp/node-3/config.json",
            "a per-node override must win, which is what lets N nodes share a host"
        );
        assert_eq!(
            Config::resolve_location(Some("/tmp/node-3/env".into()), Config::ENV_FILE_LOCATION),
            "/tmp/node-3/env"
        );
    }

    /// `VAR=` in a shell or compose file is a mistake, not an instruction to read
    /// a file named `""`. Treating blank as unset keeps the failure mode at the
    /// default path instead of producing a confusing `No such file or directory`.
    #[test]
    fn blank_path_override_is_treated_as_unset() {
        for blank in ["", "   ", "\t"] {
            assert_eq!(
                Config::resolve_location(Some(blank.into()), Config::ENV_FILE_LOCATION),
                "/env",
                "blank override {blank:?} must fall back to the default"
            );
        }
    }

    #[test]
    fn env_dir_loads_valid_specs_keyed_by_environment_id() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("pneumatic.log");
        std::fs::write(
            dir.path().join("env.json"),
            valid_env_spec_json(log.to_str().unwrap()),
        )
        .unwrap();

        let metas = Config::get_environment_metadata_from(dir.path())
            .expect("valid env spec must load");
        assert_eq!(metas.len(), 1);
        let env = metas.get("env").expect("keyed by environment_id");
        assert_eq!(env.environment_id, "env");
    }

    #[test]
    fn env_dir_skips_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("pneumatic.log");
        std::fs::write(
            dir.path().join("env.json"),
            valid_env_spec_json(log.to_str().unwrap()),
        )
        .unwrap();
        // The loader explicitly skips zero-length reads (e.g. editor swap files).
        std::fs::write(dir.path().join(".env.json.swp"), b"").unwrap();

        let metas = Config::get_environment_metadata_from(dir.path())
            .expect("empty file must be skipped, not fatal");
        assert_eq!(metas.len(), 1);
    }

    #[test]
    fn env_dir_rejects_out_of_range_spec() {
        // Phase 5.7 / H6: a security-relevant field outside its valid range
        // fails load (boot) — a neutered quorum must never boot.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("pneumatic.log");
        let mut spec: serde_json::Value =
            serde_json::from_str(include_str!("../deploy/config/env/env.json")).unwrap();
        spec["log_file"] = serde_json::json!(log.to_str().unwrap());
        spec["quorum_percentage"] = serde_json::json!(150.0);
        std::fs::write(dir.path().join("env.json"), spec.to_string()).unwrap();

        let err = match Config::get_environment_metadata_from(dir.path()) {
            Ok(_) => panic!("invalid quorum must fail load"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn env_dir_rejects_unparseable_spec() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("env.json"), b"nonsense not json").unwrap();
        assert!(
            Config::get_environment_metadata_from(dir.path()).is_err(),
            "an unreadable env file must fail boot"
        );
    }

    #[test]
    fn env_dir_missing_directory_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = match Config::get_environment_metadata_from(&dir.path().join("no-env-dir")) {
            Ok(_) => panic!("missing /env equivalent must fail build"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    // --- bind address / transport listen IP ---------------------------------
    // Before 10/02/2026 `ip_address` was not loadable at all (always
    // unspecified) and only the committer translated it into a bind address;
    // node-server passed none and so inherited the builder's 127.0.0.1. These
    // pin the shared rule.

    #[test]
    fn bind_address_defaults_to_unspecified() {
        let unset = Config::resolve_bind_address(None).expect("unset must bind unspecified");
        assert!(unset.is_unspecified());
        // Blank behaves like unset, matching the path-override rule above:
        // a JSON `""` is an unfilled template field, not an address.
        let blank = Config::resolve_bind_address(Some("   "))
            .expect("blank must bind unspecified, not fail");
        assert!(blank.is_unspecified());
    }

    #[test]
    fn bind_address_honors_a_configured_interface() {
        // Whitespace-tolerant: config.json files get hand-edited.
        let addr = Config::resolve_bind_address(Some(" 10.0.0.5 "))
            .expect("valid address must parse");
        assert_eq!(addr.to_string(), "10.0.0.5");
    }

    /// The fail-closed half. Falling back to "any interface" here would boot a
    /// node that looks healthy in every log while its peers record delivery
    /// failures, which is the exact failure mode this whole area keeps producing.
    #[test]
    fn malformed_bind_address_stops_boot_and_names_the_value() {
        let err = Config::resolve_bind_address(Some("10.0.0.256"))
            .expect_err("a malformed ip_address must not be ignored");
        assert!(
            err.message.contains("10.0.0.256"),
            "the error must quote the offending value, got: {}",
            err.message
        );
    }

    #[test]
    fn rns_listen_ip_binds_every_interface_when_unspecified() {
        let mut config = Config::new_for_testing(
            "env".to_string(),
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
        );
        config.ip_address = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
        // This is what makes a composite reachable from another host. Asserting
        // the literal, because "0.0.0.0" vs "127.0.0.1" is the entire bug.
        assert_eq!(config.rns_listen_ip(), "0.0.0.0");
    }

    #[test]
    fn rns_listen_ip_uses_the_configured_interface_verbatim() {
        let mut config = Config::new_for_testing(
            "env".to_string(),
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
        );
        config.ip_address = "10.0.0.5".parse().expect("static addr");
        assert_eq!(config.rns_listen_ip(), "10.0.0.5");
    }

    /// An `ip_address` in config.json reaches the spec, and its absence does
    /// not change existing files' behavior.
    #[test]
    fn config_spec_ip_address_is_optional_and_loads() {
        let dir = tempfile::tempdir().unwrap();

        let plain = dir.path().join("config.json");
        std::fs::write(&plain, base_spec().to_string()).unwrap();
        assert!(
            Config::load_spec_from(&plain).expect("parse").ip_address.is_none(),
            "existing config.json files must keep binding as before"
        );

        let mut with_ip = base_spec();
        with_ip["ip_address"] = serde_json::json!("10.0.0.5");
        let bound = dir.path().join("config-bound.json");
        std::fs::write(&bound, with_ip.to_string()).unwrap();
        assert_eq!(
            Config::load_spec_from(&bound).expect("parse").ip_address.as_deref(),
            Some("10.0.0.5")
        );
    }
}