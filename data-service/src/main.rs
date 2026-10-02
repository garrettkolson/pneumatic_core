//! `pneumatic_data_service` — run the data service a pneumatic cluster needs.
//!
//! The side-car both node binaries fail closed without. Configuration comes
//! from the environment (same convention as the node binaries) with optional
//! CLI overrides:
//!
//! | Input | Default | Meaning |
//! |---|---|---|
//! | `PNEUMATIC_DATA_ADDR` / `--listen` | `127.0.0.1:55555` | TCP address to serve. The same variable names the *address to dial* on the node side, so a testnet sets it once and both halves agree. Use `:0` to bind any free port (the chosen port is printed). |
//! | `PNEUMATIC_DATA_SECRET` / `--secret` | unset | HMAC-SHA256 shared secret. Unset = the legacy unauthenticated framing (dev only; the nodes warn at boot). |
//! | `PNEUMATIC_GENESIS` / `--genesis` | unset | Path to a genesis JSON. When set, genesis is applied over the channel after the listener is up (envelopes built by the client API). |
//! | `PNEUMATIC_DATA_STATE` / `--state` | unset | Persist the store here (JSON, write-through) and load it at start. Unset = in-memory only, which is right for a throwaway testnet. |
//!
//! Signals: no drain sequence is needed. Writes are persisted per `put`, so a
//! SIGTERM between requests loses nothing an already-acknowledged write had
//! promised.
//!
//! ```text
//! PNEUMATIC_DATA_ADDR=127.0.0.1:55555 \
//! PNEUMATIC_GENESIS=deploy/config/testnet/genesis.example.json \
//! cargo run -p pneumatic_data_service --bin pneumatic_data_service
//! ```

use std::env;
use std::net::SocketAddr;
use std::process::ExitCode;

use pneumatic_core::conns::ConnTarget;
use pneumatic_core::data::DefaultDataProvider;

use pneumatic_data_service::{apply_genesis, load_spec, spawn, DataStore};

const DEFAULT_LISTEN: &str = "127.0.0.1:55555";

/// One CLI flag value taken from `argv` (`--flag value` or `--flag=value`).
fn flag(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix(&format!("--{name}=")) {
            return Some(rest.to_string());
        }
        if *arg == format!("--{name}") {
            return iter.next().cloned();
        }
    }
    None
}

/// CLI flag first, then the environment variable, then `fallback`.
fn resolve(args: &[String], name: &str, env_var: &str, fallback: &str) -> String {
    flag(args, name)
        .or_else(|| env::var(env_var).ok())
        .unwrap_or_else(|| fallback.to_string())
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "usage: pneumatic_data_service [--listen ADDR] [--secret SECRET] \
             [--genesis FILE] [--state FILE]\n\n\
             Data service for pneumatic nodes (framed MsgPack, one request per \
             connection).\nEnvironment equivalents: PNEUMATIC_DATA_ADDR, \
             PNEUMATIC_DATA_SECRET,\nPNEUMATIC_GENESIS, PNEUMATIC_DATA_STATE."
        );
        return ExitCode::SUCCESS;
    }

    let listen = resolve(&args, "listen", "PNEUMATIC_DATA_ADDR", DEFAULT_LISTEN);
    let listen: SocketAddr = match listen.parse() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("[data-service] invalid listen address {listen:?}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let secret = flag(&args, "secret")
        .or_else(|| env::var("PNEUMATIC_DATA_SECRET").ok())
        .map(String::into_bytes);
    let genesis_path = flag(&args, "genesis").or_else(|| env::var("PNEUMATIC_GENESIS").ok());
    let state_path = flag(&args, "state").or_else(|| env::var("PNEUMATIC_DATA_STATE").ok());

    // 1. Load (or create) the store before accepting traffic, so the first
    //    request after the listener opens sees the persisted state rather than
    //    an empty map that a node would read as a corrupt pool.
    let store = match state_path {
        Some(ref path) => match DataStore::with_state_file(path) {
            Ok(store) => {
                println!("[data-service] loaded {} record(s) from {path}", store.len());
                std::sync::Arc::new(store)
            }
            Err(e) => {
                eprintln!("[data-service] cannot load state file {path}: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => std::sync::Arc::new(DataStore::new()),
    };

    // 2. Start accepting. `spawn` resolves an ephemeral `:0` bind to the real
    //    port, which is what a testnet launcher running several clusters on one
    //    host needs.
    let (bound, accept_thread) = match spawn(listen, store.clone(), secret.clone()) {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("[data-service] cannot bind {listen}: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "[data-service] serving {bound} ({} record(s), auth: {})",
        store.len(),
        if secret.is_some() { "HMAC-SHA256" } else { "none — dev framing" }
    );

    // 3. Genesis, applied over the channel as an ordinary client. Doing it this
    //    way means the stake-snapshot and pool envelopes are written by
    //    `pneumatic_core`'s own save path, so a genesis record is byte-identical
    //    to one a running committer would persist.
    if let Some(path) = &genesis_path {
        let spec = match load_spec(path) {
            Ok(spec) => spec,
            Err(e) => {
                eprintln!("[data-service] {e}");
                return ExitCode::FAILURE;
            }
        };
        let mut provider = DefaultDataProvider::new()
            .with_source(ConnTarget::Remote(bound));
        if let Some(secret) = secret.clone() {
            provider = provider.with_secret(secret);
        }
        match apply_genesis(&spec, &provider) {
            Ok(report) => println!("[data-service] genesis applied: {report}"),
            Err(e) => {
                eprintln!("[data-service] genesis failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        println!("[data-service] no genesis spec given — the store stays as-is (a cluster will not boot against an empty store)");
    }

    // 4. Block until the accept loop ends. In practice that is never: `serve`
    //    loops over `incoming()` and survives individual failures, so the
    //    process is torn down by a signal.
    let _ = accept_thread.join();
    ExitCode::SUCCESS
}
