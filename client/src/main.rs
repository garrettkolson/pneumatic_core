//! `pneumatic-tx` — submit transactions through a node's ingress (ADR-020).
//!
//! Thin CLI over [`pneumatic_client::TxClient`]. It exists so a human (or the
//! rollout runbook) can drive a live cluster from a shell: one transfer, or a
//! sustained stream with `--repeat N` for the Phase-1 exit test. It performs
//! exactly what the library does — sign as the sender, POST the `Process`
//! envelope — and nothing the library would not.
//!
//! ```text
//! pneumatic-tx submit --addr 127.0.0.1:9400 --chain-id env \
//!     --token 01 --to 09 --amount 100 --nonce 1 --repeat 5
//! ```
//!
//! The account is derived from `--seed` (32 bytes, hex) so repeat runs are
//! reproducible and the operator never handles a random ephemeral key they
//! cannot look up in the data service afterwards.

use std::process::ExitCode;
use std::sync::Arc;

use pneumatic_client::{ClientError, TxClient};
use pneumatic_core::crypto::Ed25519Provider;

const USAGE: &str = "\
pneumatic-tx — submit a transaction through a node's ingress (ADR-020)

USAGE:
    pneumatic-tx submit --addr <ip:port> --chain-id <id> --token <hex> --to <hex>
                        --amount <n> --nonce <n> [options]

SUBMIT OPTIONS:
  --addr <ip:port>       ingress address (required)
  --chain-id <id>        environment / chain id (required; must match the node)
  --token <hex>          token id, hex (required)
  --to <hex>             receiver public key, hex (required)
  --amount <n>           transfer amount, u64 (required)
  --nonce <n>           starting sequence number (required)
  --seed <hex32>         32-byte sender seed, hex (default: a fixed dev seed)
  --repeat <n>           submit n transfers, incrementing nonce and tx-id (default 1)
  --id-prefix <s>        tx-id prefix (default: 'tx')
  --interval-ms <n>      sleep between repeat submissions (default 0)

The account is derived from --seed; without it a fixed dev seed is used, which
is for local testnets only — never a network with value.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") || args.is_empty() {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    // One current-thread runtime: the CLI submits sequentially, and the
    // client crate already pins tokio with the runtime features.
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[pneumatic-tx] runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(&args)) {
        Ok(count) => {
            eprintln!("[pneumatic-tx] submitted {count} transaction(s) and all were accepted");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[pneumatic-tx] error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parse flags, drive the client, and report how many submissions were
/// accepted before the first failure (a failure aborts the run — a partial
/// stream is not a sustained-traffic pass).
async fn run(args: &[String]) -> Result<usize, String> {
    let sub = args.first().map(String::as_str).ok_or("missing subcommand (see --help)")?;
    if sub != "submit" {
        return Err(format!("unknown subcommand {sub:?} (see --help)"));
    }

    let addr = flag(args, "addr").ok_or("--addr is required")?;
    let addr: std::net::SocketAddr = addr.parse().map_err(|e| format!("--addr {addr}: {e}"))?;
    let chain_id = flag(args, "chain-id").ok_or("--chain-id is required")?;
    let token = hex::decode(flag(args, "token").ok_or("--token is required (hex)")?.replace("0x", ""))
        .map_err(|e| format!("--token hex: {e}"))?;
    let to = hex::decode(flag(args, "to").ok_or("--to is required (hex)")?.replace("0x", "")).map_err(|e| format!("--to hex: {e}"))?;
    let amount = parsed::<u64>(args, "amount")?.ok_or("--amount is required")?;
    let nonce0 = parsed::<usize>(args, "nonce")?.ok_or("--nonce is required")?;
    let repeat = parsed::<usize>(args, "repeat")?.unwrap_or(1);
    let id_prefix = flag(args, "id-prefix").unwrap_or("tx").to_string();
    let interval_ms = parsed::<u64>(args, "interval-ms")?.unwrap_or(0);

    // A fixed, obvious dev seed by default: local testnets only. A caller who
    // cares passes --seed and gets a stable account they can find in the data
    // service by its public key.
    let seed: [u8; 32] = match flag(args, "seed") {
        Some(s) => {
            let bytes = hex::decode(s.replace("0x", "")).map_err(|e| format!("--seed hex: {e}"))?;
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| format!("--seed must be 32 bytes, got {}", bytes.len()))?
        }
        None => [0x42u8; 32],
    };
    let account = Arc::new(Ed25519Provider::from_seed(seed));
    let client = TxClient::new(addr, chain_id, account.clone());
    eprintln!(
        "[pneumatic-tx] submitting as {} to {addr} (chain {chain_id})",
        hex::encode(client.public_key().map_err(|e| e.to_string())?)
    );

    let mut accepted = 0usize;
    for i in 0..repeat {
        let tx_id = format!("{id_prefix}-{}", nonce0 + i);
        let seq = nonce0 + i;
        match client.submit_transfer(tx_id.clone(), &token, to.clone(), amount, seq).await {
            Ok(receipt) => {
                println!("accepted {}", receipt.tx_id);
                accepted += 1;
            }
            Err(e) => {
                // Surface the node's honest reason, then stop: a stream that
                // hit a refusal is not a clean sustained-traffic result.
                let detail = match &e {
                    ClientError::Rejected { status, reason } => format!("{status} {reason}"),
                    other => other.to_string(),
                };
                return Err(format!("submission {tx_id} after {accepted} accepted: {detail}"));
            }
        }
        if interval_ms > 0 && i + 1 < repeat {
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
        }
    }
    Ok(accepted)
}

// `main` drives the async `run` through a runtime built inline (see above);
// no `#[tokio::main]` needed since we manage the ExitCode by hand.

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == &format!("--{name}"))?;
    args.get(i + 1).map(String::as_str)
}

fn parsed<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>, String> {
    match flag(args, name) {
        Some(v) => v
            .parse::<T>()
            .map(Some)
            .map_err(|_| format!("--{name}: cannot parse {v:?}")),
        None => Ok(None),
    }
}
