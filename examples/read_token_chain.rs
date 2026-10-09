//! Read a token's chain state through a **live data service**.
//!
//! The operational twin of the ingress exit test's terminal assertion: given
//! a running cluster's data service, what does the committed chain look like
//! through the exact read path (`DefaultDataProvider`) the nodes use? The
//! Phase 2 runbook uses it as the commit-observable check: a submitted
//! transaction has *delivered* only when the token chain on the committer's
//! sidecar grew, not when a log line appeared.
//!
//! ```text
//! cargo run --example read_token_chain -- 127.0.0.1:55555 0a token
//! ```

use pneumatic_core::conns::ConnTarget;
use pneumatic_core::data::DefaultDataProvider;
use pneumatic_core::data::DataProvider;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [addr, token_hex, partition] = [
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
        args.get(2).map(String::as_str),
    ] else {
        eprintln!("usage: read_token_chain <data-service host:port> <token-id hex> <partition>");
        return std::process::ExitCode::FAILURE;
    };
    let (Some(addr), Some(token_hex), Some(partition)) = (addr, token_hex, partition) else {
        eprintln!("usage: read_token_chain <data-service host:port> <token-id hex> <partition>");
        return std::process::ExitCode::FAILURE;
    };
    let addr: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bad address {addr}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let token_id = match hex::decode(token_hex.replace("0x", "")) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("bad token hex: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let provider = DefaultDataProvider::new().with_source(ConnTarget::Remote(addr));
    match provider.get_token(&token_id, partition) {
        Ok(token) => {
            let state = token.blockchain.get_current_chain_state();
            // `blocks` alone cannot answer "is the chain growing", and the
            // 10/08/2026 rehearsal is the proof: a token's chain is a sliding
            // window (`Token::security_level`, default 5) — once it is full,
            // every commit trims the oldest block and appends the new one, so
            // the COUNT sits at the window size forever while the chain
            // advances underneath it. `sequence` is the counter that answers
            // the question: `Token::commit_block` bumps it exactly once per
            // committed block and never on a trim, so it is monotonic in
            // commits regardless of the window. `key=value` so a caller can
            // take the field it means instead of counting words.
            println!(
                "token={} partition={partition} blocks={} sequence={} tip={}",
                hex::encode(&token.id),
                token.blockchain.get_count(),
                token.sequence_number,
                hex::encode(&state.last_hash_in),
            );
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            // A miss is a real answer with a non-zero exit: an absent token is
            // how "the transaction never got here" shows up.
            eprintln!("token {} absent or unreadable: {e:?}", hex::encode(&token_id));
            std::process::ExitCode::FAILURE
        }
    }
}
