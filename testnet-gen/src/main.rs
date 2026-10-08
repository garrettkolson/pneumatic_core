//! `pneumatic_testnet_gen` — write a bootable multi-node testnet to disk.
//!
//! Emits, for N nodes across the four roles:
//!
//! | Artifact | Why it has to be generated rather than hand-written |
//! |---|---|
//! | `nodes/<name>/node_identity.json` | Peers must know each other's RNS public key at seed time, so keys exist before first boot. Written by the loader's own writer so the hybrid PQC format cannot drift. |
//! | `nodes/<name>/config.json` | `bootstrap_peers` carries the *other side's* base-plus-`j` port. No node can compute that alone. |
//! | `nodes/<name>/env/env.json` | The env spec's `log_file` is one absolute path; N nodes sharing it interleaves. |
//! | `genesis.json` | Must list each node's **Ed25519** key, while configs carry its **RNS** key. Swapping them boots a node that installs no roles. |
//! | `manifest.json` | One file an operator or launcher can read to see every key, port, and peer link. |
//!
//! ```text
//! # Four validators, one per role, into /tmp/testnet:
//! cargo run -p pneumatic_testnet_gen --bin pneumatic_testnet_gen -- --out /tmp/testnet
//!
//! # 40 validators (10 per role), role-graph peer sets:
//! cargo run -p pneumatic_testnet_gen --bin pneumatic_testnet_gen -- \
//!     --out /tmp/testnet --validators 40 --topology role-graph
//! ```
//!
//! It writes files and starts nothing. Booting them is the launcher's job
//! (`task-testnet-launcher`); the printed epilogue shows the exact env vars each
//! node needs in the meantime.

use std::path::PathBuf;
use std::process::ExitCode;

use pneumatic_testnet_gen::cli::{flag, flag_all, parsed};
use pneumatic_testnet_gen::emit::{GenSpec, Report};
use pneumatic_testnet_gen::topology::{Placement, Role, TopologyMode};

const USAGE: &str = "\
pneumatic_testnet_gen — generate a bootable multi-node testnet

  --out <dir>              output directory (default: deploy/generated/testnet)
  --validators <n>         total nodes, split across the four roles (leftover
                           roles come first). Individual role flags override it.
  --sentinels <n>          sentinel nodes
  --executors <n>          executor nodes
  --finalizers <n>         finalizer nodes
  --committers <n>         committer nodes
  --base-port <n>          first UDP port (default: 21000)
  --topology <mode>        full-mesh | role-graph | auto (default: full-mesh;
                           auto switches to role-graph above --role-graph-above)
  --role-graph-above <n>   threshold for --topology auto (default: 8)
  --stake <n>             validator stake in genesis (default: 1000)
  --fuel <n>              validator fuel balance (default: 1000000)
  --data-addr <host:port>  where the data service will listen (default: 127.0.0.1:55555)
  --env-template <path>    env spec to clone per node (default: deploy/config/env/env.json)
  --addresses <a,b,c,...>  one address per node, in report order. Supplying these
                           means one node per machine: peers dial these addresses
                           and every host shares ONE port range.
  --addresses-file <path>  same, one address per line; blank lines and #-comments
                           ignored. Feed a provisioner's output straight in.
  --bind-ip <ip>           address each node binds. Default: every interface.
                           Set on a multi-homed host, where binding every
                           interface would answer on the wrong network.
  --tx-token <hex>         token id to seed in genesis so submitted
                            transactions name a token that exists (repeatable).
  --client-account <hex>   non-validator account public key to fund in
                            genesis (repeatable) — the pneumatic-tx sender.
  --force-keys             replace existing keystores (orphans their genesis stake)
  --help                   this text

One node per machine (a cloud testnet), where a provisioner has already produced
addresses. Note the two-pass order: keystores first, because a node's config
lists its peers' public keys, so identities must exist before configs can be
written — then addresses, which only exist after provisioning.

  pneumatic_testnet_gen --out /tmp/keys --validators 4
  # ...provision 16 machines, collect their addresses into addresses.txt...
  pneumatic_testnet_gen --out /tmp/keys --validators 4 --addresses-file addresses.txt
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    match run(&args) {
        Ok(report) => {
            print_report(&report);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[testnet-gen] error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<Report, String> {
    let mut spec = GenSpec::new(PathBuf::from("deploy/generated/testnet"));
    if let Some(out) = flag(args, "out") {
        spec.out_dir = PathBuf::from(out);
    }
    if let Some(port) = parsed::<u16>(args, "base-port")? {
        spec.base_port = port;
    }
    if let Some(threshold) = parsed::<usize>(args, "role-graph-above")? {
        spec.role_graph_above = threshold;
    }
    if let Some(stake) = parsed::<u64>(args, "stake")? {
        spec.stake = stake;
    }
    if let Some(fuel) = parsed::<u64>(args, "fuel")? {
        spec.fuel_balance = fuel;
    }
    if let Some(addr) = flag(args, "data-addr") {
        spec.data_addr = addr;
    }
    if let Some(template) = flag(args, "env-template") {
        spec.env_template = PathBuf::from(template);
    }
    // Where the nodes run. Supplying addresses *is* the declaration that this is
    // not a single-host cluster, so there is no separate `--per-host` flag that
    // could contradict the address list.
    let addresses = match (flag(args, "addresses"), flag(args, "addresses-file")) {
        (Some(_), Some(_)) => {
            return Err("--addresses and --addresses-file are mutually exclusive".to_string())
        }
        (Some(list), None) => list
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect(),
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map_err(|e| format!("--addresses-file {path}: {e}"))?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect(),
        (None, None) => Vec::new(),
    };
    if !addresses.is_empty() {
        spec.placement = Placement::PerHost { addresses };
    }
    if let Some(bind) = flag(args, "bind-ip") {
        spec.bind_address = Some(bind);
    }
    spec.force_keys = args.iter().any(|a| a == "--force-keys");
    // Traffic artifacts: the token ids submitted transactions will name, and
    // the accounts that submit them. Both must be in genesis before the first
    // transaction, because nothing on the wire creates either.
    spec.tx_tokens = flag_all(args, "tx-token");
    spec.client_accounts = flag_all(args, "client-account");

    match flag(args, "topology").as_deref() {
        None | Some("full-mesh") => spec.mode = TopologyMode::FullMesh,
        Some("role-graph") => spec.mode = TopologyMode::RoleGraph,
        Some("auto") => spec.mode = TopologyMode::Auto,
        Some(other) => {
            return Err(format!(
                "--topology must be full-mesh, role-graph or auto, got {other:?}"
            ))
        }
    }

    // Counts. `--validators n` is n nodes *total* split across the four roles;
    // individual role flags override it, so `--validators 40 --sentinels 6` means
    // 6 sentinels and 10 of everything else.
    let mut counts = spec.counts;
    if let Some(n) = parsed::<usize>(args, "validators")? {
        // The remainder is handed out in role order rather than truncated. The
        // old `n / 4` made `--validators 10` silently build 8 nodes, and made
        // `--validators 1..3` fail as "every role count is zero" — both are the
        // sort of quiet surprise this generator exists to remove. A count below
        // 4 now means what it says: a cluster that lacks some roles.
        for (i, (_, count)) in counts.iter_mut().enumerate() {
            *count = n / 4 + usize::from(i < n % 4);
        }
    }
    for (role, flag_name) in [
        (Role::Sentinel, "sentinels"),
        (Role::Executor, "executors"),
        (Role::Finalizer, "finalizers"),
        (Role::Committer, "committers"),
    ] {
        if let Some(n) = parsed::<usize>(args, flag_name)? {
            let slot = counts
                .iter_mut()
                .find(|(r, _)| *r == role)
                .ok_or_else(|| format!("unknown role {role:?}"))?;
            slot.1 = n;
        }
    }
    spec.counts = counts;

    pneumatic_testnet_gen::emit::generate(&spec)
}

/// Per-process cost of one node, from the transport's own thread model:
/// `WORKER_THREADS` (4) + 1 announce worker + 2 threads per UDP interface.
fn thread_estimate(interfaces: usize) -> usize {
    4 + 1 + 2 * interfaces
}

fn print_report(report: &Report) {
    println!("pneumatic testnet generated at {}", report.out_dir);
    println!(
        "  topology {:?} · {} nodes · {} keystores created, {} reused",
        report.mode,
        report.nodes.len(),
        report.keystore_created,
        report.keystore_reused
    );
    // The split, because `--validators 10` is 3/3/2/2 and an operator should see
    // that rather than infer it from the table — role counts change the mesh.
    let split: Vec<String> = [
        (Role::Sentinel, "sentinel"),
        (Role::Executor, "executor"),
        (Role::Finalizer, "finalizer"),
        (Role::Committer, "committer"),
    ]
    .iter()
    .map(|(role, label)| {
        let n = report
            .nodes
            .iter()
            .filter(|n| &n.role == role)
            .count();
        format!("{label} {n}")
    })
    .collect();
    println!("  role split: {}\n", split.join(" · "));
    println!(
        "  {:<14} {:<10} {:<15} {:>6} {:>5} {:>8}  peers",
        "node", "role", "peer dials", "port", "links", "threads"
    );
    println!("  {}", "-".repeat(92));
    for node in &report.nodes {
        println!(
            "  {:<14} {:<10} {:<15} {:>6} {:>5} {:>8}  {}",
            node.name,
            format!("{:?}", node.role).to_lowercase(),
            node.address,
            node.rns_port,
            node.interfaces,
            thread_estimate(node.interfaces),
            node.peers.join(" ")
        );
    }
    println!();
    println!("  cost ({} placement):", report.placement);
    println!(
        "    max UDP interfaces per node: {} (≈{} threads/process)",
        report.max_interfaces,
        thread_estimate(report.max_interfaces)
    );
    println!(
        "    UDP ports per machine:       {}{}",
        report.ports_per_host,
        if report.placement == "per-host" {
            "  (one range reused on every host)"
        } else {
            ""
        }
    );
    println!(
        "    total UDP sockets:           {}",
        report.total_interfaces
    );
    if report.placement == "per-host" {
        let lo = report.base_port;
        let hi = report.base_port + report.ports_per_host - 1;
        println!(
            "    firewall: open UDP {lo}-{hi} between cluster members only. Leaves cannot\n    route through other leaves, so this range must be reachable peer-to-peer: a\n    load balancer or NAT gateway cannot stand in for it."
        );
    }
    if report.max_interfaces > 12 {
        println!(
            "    NOTE: {} interfaces/node is past anything this repo has run. The \
             suite's own\ndense-mesh test documents directed routes that never go \
             live; expect to shorten\nthe mesh or exercise relay before blaming \
             peering.",
            report.max_interfaces
        );
    }
    println!();
    println!("  genesis: {}", report.genesis);
    println!("           (Ed25519 keys — bootstrap_peers carry RNS keys; they are not interchangeable)");
    println!("  shielded_root_recency: {} (copied from the env template; genesis must match it)",
             report.shielded_root_recency);
    println!();
    println!("  boot one node with:");
    println!("    cd {} && \\", report.nodes[0].dir);
    println!("      PNEUMATIC_CONFIG_FILE=./config.json \\");
    println!("      PNEUMATIC_ENV_DIR=./env \\");
    println!("      PNEUMATIC_DATA_ADDR={} \\", report.data_addr);
    println!("      cargo run -p pneumatic_node_server --bin node-server");
    println!("  (the data service must already be listening, or every node fails closed at boot)");
}
