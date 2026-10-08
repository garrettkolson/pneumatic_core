//! Emitters: keystores, per-node `config.json`, per-node env dir, `genesis.json`,
//! and the operator-facing manifest.
//!
//! The ordering is load-bearing. Keystores are created in a **first pass**,
//! because a node's `config.json` lists its peers' hex RNS public keys, which do
//! not exist until every peer's identity has been generated. Configs are written
//! in a second pass. Both passes read the same [`Mesh`], which is the only
//! place the j-rule is resolved.

use std::fs;
use std::path::{Path, PathBuf};

use pneumatic_core::config::{meets_minimum_stake, Config};
use pneumatic_core::crypto::AsymCryptoProvider;
use pneumatic_core::environment::CostModel;
use pneumatic_core::rns::identity::NodeIdentity;
use serde::Serialize;

use crate::topology::{Mesh, NodePlan, Placement, Role, TopologyMode};

/// The role set a composite with `stake` will install and declare at boot —
/// the same qualification the composite applies (`meets_minimum_stake`: the
/// stake must clear BOTH the protocol-global floor and the role's own floor).
/// The floors are read from the same env template the nodes boot with,
/// falling back to the same defaults the composite falls back to, so the
/// manifest records what the nodes will actually run — not what they are
/// named after. A single-role manifest over a composite genesis is a fiction:
/// the peers' directories disagree with it, and mesh-probe rightly reports
/// the honest composite as wrong-bucket 36 ways at once.
fn qualifying_roles(stake: u64, template: &serde_json::Value) -> Vec<Role> {
    let cost_model = template.get("cost_model");
    let global_min = cost_model
        .and_then(|c| c.get("global_min_stake"))
        .and_then(|v| v.as_u64())
        .unwrap_or_else(CostModel::default_global_min_stake);
    Role::ALL
        .into_iter()
        .filter(|role| {
            let type_min = cost_model
                .and_then(|c| c.get("per_type_min_stake"))
                .and_then(|m| m.get(role.registry_type_str()))
                .and_then(|v| v.as_u64())
                .unwrap_or_else(Config::default_min_stake);
            meets_minimum_stake(stake, global_min, type_min)
        })
        .collect()
}

/// Everything the generator needs. Defaults are chosen so that
/// `pneumatic_testnet_gen --out /tmp/testnet` produces something bootable.
#[derive(Clone, Debug)]
pub struct GenSpec {
    pub counts: [(Role, usize); 4],
    pub out_dir: PathBuf,
    pub base_port: u16,
    pub mode: TopologyMode,
    /// Above this node count, `--topology auto` picks the role graph.
    pub role_graph_above: usize,
    pub stake: u64,
    pub fuel_balance: u64,
    /// The env spec every node clones, then rewrites `log_file` in.
    pub env_template: PathBuf,
    pub environment_id: String,
    pub token_partition_id: String,
    /// Where the data service listens; recorded in the manifest so the launcher
    /// and the operator see the same value. Not part of `config.json` — the
    /// nodes take it from `PNEUMATIC_DATA_ADDR`.
    pub data_addr: String,
    /// Where the nodes run: one host (loopback peers, disjoint port windows) or
    /// one node per machine (real addresses, one shared port range).
    pub placement: Placement,
    /// Address each node binds its transport to. `None` binds every interface,
    /// which is what the committer already does and the right default. Set it on
    /// a multi-homed instance, where binding "any" answers on the wrong network
    /// and every peer just records a delivery failure.
    pub bind_address: Option<String>,
    /// Regenerate keystores that already exist. Off by default: replacing a
    /// keystore orphans the stake that genesis put under the old key, so a
    /// re-run must be explicit.
    pub force_keys: bool,
    /// Transfer-token ids (hex) to seed in genesis, so submitted transactions
    /// name a token that exists. Empty = none (the historical shape).
    pub tx_tokens: Vec<String>,
    /// Non-validator account public keys (hex) to fund in genesis — usually
    /// the `pneumatic-tx` sender, whose key derives from its seed and must be
    /// a genesis account before it can submit anything.
    pub client_accounts: Vec<String>,
}

impl GenSpec {
    pub fn new(out_dir: PathBuf) -> Self {
        GenSpec {
            // Four validators, one per role: the smallest topology that exercises
            // every pipeline hop and every role directory.
            counts: [
                (Role::Sentinel, 1),
                (Role::Executor, 1),
                (Role::Finalizer, 1),
                (Role::Committer, 1),
            ],
            out_dir,
            base_port: 21_000,
            mode: TopologyMode::FullMesh,
            role_graph_above: 8,
            // 10 is the registration floor (`Config::default_min_stake`); 1000
            // clears every role floor with headroom.
            stake: 1000,
            fuel_balance: 1_000_000,
            env_template: PathBuf::from("deploy/config/env/env.json"),
            environment_id: "env".to_string(),
            token_partition_id: "token".to_string(),
            data_addr: "127.0.0.1:55555".to_string(),
            placement: Placement::SingleHost,
            bind_address: None,
            force_keys: false,
            tx_tokens: Vec::new(),
            client_accounts: Vec::new(),
        }
    }

    /// `--topology auto`: full mesh while the interface count stays modest, role
    /// graph beyond it. The role graph is not a density fix (see
    /// `topology::FANOUT`); past the threshold it at least stops asking each
    /// executor to bind links to its peers.
    pub fn resolve_mode(&self) -> TopologyMode {
        // Only `auto` escalates. `--topology full-mesh` at 40 validators means
        // 40-validator full mesh, cost report and all.
        let total: usize = self.counts.iter().map(|(_, c)| c).sum();
        self.mode.resolve(total, self.role_graph_above)
    }
}

/// One generated node, as reported and as recorded in `manifest.json`.
#[derive(Clone, Debug, Serialize)]
pub struct NodeReport {
    pub name: String,
    pub role: Role,
    /// Every role this node runs. A composite installs one plugin per role its
    /// stake qualifies for, so the manifest records the qualifying set — not
    /// the naming role — because that is what the node declares to peers and
    /// what mesh-probe must expect in every directory. A cluster whose
    /// genesis pays below every floor would run no roles at all; generation
    /// refuses (see `qualifying_roles`).
    pub roles: Vec<Role>,
    pub dir: String,
    /// Where peers dial this node. A launcher reads this to place the node on
    /// the right machine and to open the right firewall range.
    pub address: String,
    pub rns_port: u16,
    pub interfaces: usize,
    pub peers: Vec<String>,
    pub rhash_hex: String,
    pub ed25519_public_key_hex: String,
    pub rns_public_key_hex: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub out_dir: String,
    pub mode: TopologyMode,
    pub nodes: Vec<NodeReport>,
    pub genesis: String,
    pub max_interfaces: usize,
    pub total_interfaces: usize,
    /// UDP ports one machine must expose. Differs from `total_interfaces`
    /// because a per-host placement reuses one range on every machine.
    pub ports_per_host: u16,
    /// First UDP port of every node's range. With per-host placement this is the
    /// same on every machine, so `base_port .. base_port + ports_per_host - 1`
    /// is literally the firewall range.
    pub base_port: u16,
    pub placement: String,
    pub keystore_reused: usize,
    pub keystore_created: usize,
    pub shielded_root_recency: usize,
    /// Where the launcher must have the data service listening before any node
    /// boots. Echoed in the epilogue so it is never guessed at.
    pub data_addr: String,
}

struct GeneratedIdentity {
    rhash_hex: String,
    ed25519_hex: String,
    rns_hex: String,
}

/// Generate the whole testnet. Idempotent for keys (see
/// [`GenSpec::force_keys`]); always rewrites configs, which are derived.
pub fn generate(spec: &GenSpec) -> Result<Report, String> {
    let out_dir = if spec.out_dir.is_absolute() {
        spec.out_dir.clone()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("resolve output directory: {e}"))?
            .join(&spec.out_dir)
    };
    fs::create_dir_all(&out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;

    let mode = spec.resolve_mode();
    let mesh = Mesh::build(spec.counts, spec.base_port, mode, spec.placement.clone())?;

    let template = read_env_template(&spec.env_template)?;

    // --- Pass 1: identities. Must precede configs: a config lists its peers'
    //     RNS public keys, which only exist once every identity exists. ---
    let mut identities: Vec<GeneratedIdentity> = Vec::with_capacity(mesh.plan.len());
    let mut reused_keys = 0usize;
    for plan in &mesh.plan {
        let dir = node_dir(&out_dir, &plan.node.name);
        fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        if spec.force_keys {
            let keystore = dir.join("node_identity.json");
            if keystore.exists() {
                fs::remove_file(&keystore)
                    .map_err(|e| format!("remove {}: {e}", keystore.display()))?;
            }
        }
        if dir.join("node_identity.json").exists() {
            reused_keys += 1;
        }
        identities.push(create_identity(&dir)?);
        // One env dir per node: the env spec's `log_file` is a single absolute
        // path, and N nodes writing one file interleaves into unreadable mush.
        write_node_env_dir(&dir, &template, spec)?;
    }

    // --- Pass 2: config.json, which needs the whole identity set. ---
    // The role set every node runs is a stake derivation, not a label: the
    // composite installs one plugin per role its stake qualifies for. Since
    // genesis pays every validator the same `spec.stake`, the qualifying set
    // is the same for all of them. A stake that qualifies for nothing would
    // boot every node running no role — a cluster that forms no pipeline — so
    // refuse it here rather than emit a fiction the probe must later call out.
    let runs_roles = qualifying_roles(spec.stake, &template);
    if runs_roles.is_empty() {
        return Err(format!(
            "genesis stake {} clears no role floor (global + per-type): every node \
             would boot running no role. Raise --stake or lower the env floors.",
            spec.stake
        ));
    }
    let mut nodes = Vec::new();
    for (index, plan) in mesh.plan.iter().enumerate() {
        let dir = node_dir(&out_dir, &plan.node.name);
        let config = build_config_json(&mesh, &identities, plan, spec, &dir)?;
        write_json(&dir.join("config.json"), &config)?;
        let identity = &identities[index];
        nodes.push(NodeReport {
            name: plan.node.name.clone(),
            role: plan.node.role,
            roles: runs_roles.clone(),
            dir: dir.display().to_string(),
            address: plan.address.clone(),
            rns_port: plan.base_port,
            interfaces: mesh.interfaces(plan.node.index),
            peers: plan.peers.iter().map(|&p| mesh.plan[p].node.name.clone()).collect(),
            rhash_hex: identity.rhash_hex.clone(),
            ed25519_public_key_hex: identity.ed25519_hex.clone(),
            rns_public_key_hex: identity.rns_hex.clone(),
        });
    }

    // --- Genesis, keyed by the Ed25519 keys the nodes are judged by. ---
    let genesis_path = out_dir.join("genesis.json");
    let recency = template
        .get("shielded_root_recency")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize;
    write_json(
        &genesis_path,
        &build_genesis_json(spec, &nodes, recency)?,
    )?;

    let manifest = serde_json::json!({
        "_about": "Generated by pneumatic_testnet_gen. Configs, env dirs and genesis are derived \
                   and safe to rewrite; node_identity.json is a keystore — deleting it orphans \
                   the stake genesis put under its key.",
        "mode": mode,
        "placement": spec.placement.label(),
        "base_port": spec.base_port,
        "ports_per_host": mesh.ports_per_host(),
        "data_addr": spec.data_addr,
        "environment_id": spec.environment_id,
        "token_partition_id": spec.token_partition_id,
        "shielded_root_recency": recency,
        "nodes": nodes,
    });
    write_json(&out_dir.join("manifest.json"), &manifest)?;

    let total_interfaces = nodes.iter().map(|n| n.interfaces).sum();
    Ok(Report {
        out_dir: out_dir.display().to_string(),
        mode,
        max_interfaces: mesh.max_interfaces(),
        total_interfaces,
        ports_per_host: mesh.ports_per_host(),
        base_port: spec.base_port,
        placement: spec.placement.label().to_string(),
        keystore_reused: reused_keys,
        keystore_created: identities.len() - reused_keys,
        nodes,
        genesis: genesis_path.display().to_string(),
        shielded_root_recency: recency,
        data_addr: spec.data_addr.clone(),
    })
}

fn node_dir(out_dir: &Path, name: &str) -> PathBuf {
    out_dir.join("nodes").join(name)
}

/// Create (or reload) one keystore.
///
/// `load_or_create` is the loader's own writer, so the on-disk format is
/// whatever the binaries expect *today* — including the ML-DSA / ML-KEM keypairs,
/// which are the persisted identity and must not be regenerated on reload. A
/// corrupt keystore is a hard error inside `load_or_create`; that is the right
/// behavior here too, because silently minting a new identity would strand the
/// genesis stake under the old key.
fn create_identity(dir: &Path) -> Result<GeneratedIdentity, String> {
    let path = dir.join("node_identity.json");
    let identity = NodeIdentity::load_or_create(&path)
        .map_err(|e| format!("keystore {}: {e}", path.display()))?;
    let rns_pub = identity
        .rns
        .get_public_key()
        .ok_or_else(|| format!("keystore {} yields no rns public key", path.display()))?;
    let ed25519_pub = identity
        .ed25519
        .public_key()
        .map_err(|e| format!("ed25519 public key for {}: {e}", path.display()))?;
    Ok(GeneratedIdentity {
        rhash_hex: hex::encode(identity.rhash),
        ed25519_hex: hex::encode(ed25519_pub),
        rns_hex: hex::encode(rns_pub),
    })
}

fn read_env_template(path: &Path) -> Result<serde_json::Value, String> {
    let raw = fs::read(path)
        .map_err(|e| format!("read env template {}: {e}", path.display()))?;
    serde_json::from_slice(&raw)
        .map_err(|e| format!("parse env template {}: {e}", path.display()))
}

/// Write the node's own env dir: the template with `log_file` repointed into the
/// node directory. Everything else — quorum, risk, shard count,
/// `shielded_root_recency` — is copied verbatim, because a cluster whose nodes
/// disagree on a consensus parameter has no failure mode better than refusing to
/// boot.
fn write_node_env_dir(dir: &Path, template: &serde_json::Value, spec: &GenSpec) -> Result<(), String> {
    let env_dir = dir.join("env");
    fs::create_dir_all(&env_dir).map_err(|e| format!("create {}: {e}", env_dir.display()))?;
    let mut spec_json = template.clone();
    let log_file = env_dir.join("pneumatic.log");
    spec_json
        .as_object_mut()
        .ok_or_else(|| "env template is not a JSON object".to_string())?
        .insert("log_file".to_string(), serde_json::Value::String(log_file.display().to_string()));
    // Keep the environment id in step with what each node's `main_env_id` says.
    spec_json["environment_id"] = serde_json::Value::String(spec.environment_id.clone());
    write_json(&env_dir.join("env.json"), &spec_json)
}

/// One node's `config.json`, in exactly the shape `ConfigSpec` parses.
fn build_config_json(
    mesh: &Mesh,
    identities: &[GeneratedIdentity],
    plan: &NodePlan,
    spec: &GenSpec,
    dir: &Path,
) -> Result<serde_json::Value, String> {
    // `j` comes from the *peer's* peer list, which is why this needs the mesh.
    let mut peers = Vec::with_capacity(plan.peers.len());
    for &peer_index in &plan.peers {
        let peer = &mesh.plan[peer_index];
        // "The port I must forward to to reach my peer" is the PEER's listen
        // port for me — the peer's base plus my index in the peer's peer list.
        // Calling this on `plan` instead yields the port the peer sends to for
        // us, which is a different number and produces a link that silently
        // never handshakes. Checked in both directions by tests/generation.rs.
        let port = peer.listen_port_for(plan.node.index)?;
        peers.push(serde_json::json!({
            "public_key": identities[peer_index].rns_hex,
            // The PEER's address, not ours and not a shared constant. On one
            // host every address is loopback so a single value looked fine;
            // across machines this is the difference between a mesh and N
            // isolated nodes.
            "ip": peer.address,
            "port": port,
        }));
    }
    let mut config = serde_json::json!({
        "is_full_node": true,
        "rest_api_version": 1,
        // Required by ConfigSpec and intentionally unused by the loader. It must
        // be present or the spec fails to parse for the wrong reason.
        "environments": [],
        "main_env_id": spec.environment_id,
        "reconciliation_partition_id": "reconciliation",
        "identity_path": dir.join("node_identity.json").display().to_string(),
        // Where this node writes its signed mesh fragment, which is how
        // `mesh-probe --fragments` sees it. Absolute like `identity_path` — so a
        // generated tree is only portable to the machine it was generated on
        // until the operator rewrites both keys (or mounts the same paths).
        // Ship this directory with whatever the log/metrics agent already tails.
        "mesh_fragment_path": dir.join("mesh_fragment.json").display().to_string(),
        "rns_port": plan.base_port,
        // Leaves only. `transport_enabled: true` makes a node a relay, which is
        // the untested lever for mesh density — the repo has never exercised it.
        "transport_enabled": false,
        "bootstrap_peers": peers,
    });
    // Omitted when unset so that existing behavior (bind every interface) is
    // the literal absence of a key rather than a value we invented.
    if let Some(bind) = spec.bind_address.as_deref().map(str::trim) {
        if !bind.is_empty() {
            config["ip_address"] = serde_json::json!(bind);
        }
    }
    Ok(config)
}

/// Genesis, keyed by **Ed25519** public keys.
///
/// This is the trap the runbook documents and the generator exists to make
/// unreachable: the key a node is judged by is its Ed25519 key, while
/// `bootstrap_peers` carries its *RNS* key. Both are 64-byte hex blobs in the
/// same manifest, and swapping them produces a node that boots, installs no
/// roles, and rejects registrations — with no error naming the cause.
fn build_genesis_json(
    spec: &GenSpec,
    nodes: &[NodeReport],
    shielded_root_recency: usize,
) -> Result<serde_json::Value, String> {
    // Validate hex at generation time, not at first boot: a mistyped token id
    // surfaced by the data service would be discovered only after the fleet is
    // up, which is the discovery class this generator exists to remove.
    for id in &spec.tx_tokens {
        hex::decode(id.trim()).map_err(|e| format!("--tx-token {id:?} is not valid hex: {e}"))?;
    }
    let mut accounts: Vec<serde_json::Value> = Vec::new();
    for key in &spec.client_accounts {
        hex::decode(key.trim())
            .map_err(|e| format!("--client-account {key:?} is not valid hex: {e}"))?;
        accounts.push(serde_json::json!({
            "public_key_hex": key.trim(),
            "fuel_balance": spec.fuel_balance,
            "stake": 0,
        }));
    }
    let validators: Vec<serde_json::Value> = nodes
        .iter()
        .map(|n| {
            serde_json::json!({
                "public_key_hex": n.ed25519_public_key_hex,
                "stake": spec.stake,
                "fuel_balance": spec.fuel_balance,
            })
        })
        .collect();
    let tokens: Vec<serde_json::Value> = spec
        .tx_tokens
        .iter()
        .map(|id| {
            serde_json::json!({
                "token_id_hex": id.trim(),
                "name": format!("testnet-token-{}", id.trim()),
            })
        })
        .collect();
    Ok(serde_json::json!({
        "_about": "Generated by pneumatic_testnet_gen. public_key_hex is each node's Ed25519 key \
                   (NOT its RNS key, which lives in the nodes' bootstrap_peers). Regenerate with \
                   the generator rather than editing by hand.",
        "environment_id": spec.environment_id,
        "token_partition_id": spec.token_partition_id,
        // Epoch 1 is read at boot; the standard pipeline path also reads epoch 0.
        "stake_snapshot_epochs": [0, 1],
        "shielded_root_recency": shielded_root_recency,
        "nodes": validators,
        "accounts": accounts,
        "tokens": tokens,
        "seed_shielded_pool": true,
        "seed_partition_token": true,
    }))
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let rendered = serde_json::to_string_pretty(value)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    fs::write(path, rendered + "\n").map_err(|e| format!("write {}: {e}", path.display()))
}
