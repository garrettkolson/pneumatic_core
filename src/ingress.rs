//! Transaction ingress (rollout roadmap Phase 1, ADR-020): the client-facing
//! HTTP surface that accepts a signed transaction into the pipeline.
//!
//! Design (see the ADR for the full rationale):
//!
//! * **No new wire type.** The POST body is the *existing* inner `Process`
//!   envelope — the exact `Message` bytes a submitting peer relays on the RNS
//!   wire (`action: "Process"`, `body`: the rmp-encoded [`Transaction`],
//!   `public_key`/`signature`: the sender's own key). HTTP here is a transport
//!   adapter in front of the same pipeline entry the nodes already speak, not
//!   a protocol change. A Phase 2 gateway, if one is ever built, relays the
//!   identical artifact.
//! * **No HTTP crate** — the same hand-rolled raw-`tokio` responder as
//!   [`crate::telemetry`], extended only as far as a POST needs: parse the
//!   request line, require `Content-Length`, read a bounded body, always
//!   answer `Connection: close`. The health responder is a GET-only scraper
//!   surface; this one accepts untrusted payloads, so every bound here is
//!   load-bearing (head size, body size, read timeout, one request per
//!   connection).
//! * **Authentication at the edge.** Before the message is handed to the
//!   submit sink, ingress re-verifies *both* signatures — the sender
//!   signature over the canonical transaction bytes (`verify_sender_signature`,
//!   the sentinel's C3 gate) and the envelope signature over the body under
//!   `public_key` (what the gossiper verifies on the RNS path) — and enforces
//!   `public_key == tx.sender`. A caller cannot enter a transaction it is not
//!   authorized for, and the sentinel re-checks the same facts downstream.
//!   Rate limiting is Phase 4; until it exists this endpoint must not be
//!   exposed beyond a loopback/localhost-trusted network (the binary binds
//!   `PNEUMATIC_INGRESS_ADDR`, which the runbook pins to loopback by default).
//! * **This module never dispatches.** The [`SubmitSink`] is injected; the
//!   node-server wires it to its `RoleDispatcher` (exactly the path the RNS
//!   bridge's `route_data_plane` uses), so ingress is indistinguishable from
//!   a peer submission from the dispatcher down. A node without a Sentinel
//!   plugin installed answers `503` — fail-closed, no silent acceptance.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::crypto::{AsymCryptoProvider, Ed25519Provider};
use crate::encoding::deserialize_rmp_to;
use crate::errors::PneumaticError;
use crate::messages::Message;
use crate::telemetry::{HealthState, Metrics};
use crate::transactions::Transaction;

/// Maximum request-body bytes accepted from one submission. A
/// `Transaction` plus its envelope is far below this in every shape
/// (hybrid signature 3.8 KB; the sentinel caps contract calldata well
/// under it); the bound exists so a hostile client cannot force an
/// unbounded allocation before authentication.
pub const MAX_INGRESS_BODY_BYTES: usize = 1024 * 1024;

/// Request-head bound, mirroring `telemetry`'s health responder.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Socket read timeout per connection (slow-loris guard).
const READ_TIMEOUT_SECS: u64 = 5;

/// The endpoint path a submission is POSTed to.
pub const INGRESS_PATH: &str = "/v1/transactions";

/// The reason a submission was not accepted, and the HTTP status it maps to.
/// The variants mirror what the pipeline can reject for, so the client sees a
/// distinct, honest signal per failure instead of one opaque 4xx.
#[derive(Debug)]
pub enum IngressError {
    /// The body did not decode as a `Process` envelope over an rmp
    /// `Transaction`, the action was not `"Process"`, the method/path/content
    /// framing was wrong, or a required field (id, sender) is absent. → `400`.
    Malformed(String),
    /// A signature failed to verify, or the envelope key is not the
    /// transaction sender. → `401`.
    Unauthenticated(String),
    /// The pipeline synchronously rejected the submission (validation,
    /// risk gate, pool full). The block is NOT committed — acceptance
    /// never means commit; commit is observed through the data service. → `422`.
    Rejected(String),
    /// The pending registry already holds this id, or the sender's nonce
    /// was used. → `409`.
    Duplicate(String),
    /// No installed role owns this action (a node without a Sentinel), or the
    /// node is draining. → `503`.
    Unavailable(String),
}

impl IngressError {
    /// The HTTP status this refusal maps to — public so wiring tests can pin the taxonomy.
    pub fn status(&self) -> u16 {
        match self {
            IngressError::Malformed(_) => 400,
            IngressError::Unauthenticated(_) => 401,
            IngressError::Rejected(_) => 422,
            IngressError::Duplicate(_) => 409,
            IngressError::Unavailable(_) => 503,
        }
    }
}

/// The future returned by a [`SubmitSink`] — boxed so it can live behind an
/// `Arc<dyn Fn>` across `tokio::spawn` (same idiom as `RoleHandler::handle`).
pub type SubmitFuture = Pin<Box<dyn Future<Output = Result<(), IngressError>> + Send>>;

/// The injected dispatch seam: receives an already-authenticated inner
/// `Process` message and routes it into the node's pipeline.
pub type SubmitSink = Arc<dyn Fn(Message) -> SubmitFuture + Send + Sync>;

/// Start the transaction-ingress HTTP server on `addr`.
///
/// * `POST /v1/transactions` — body: the rmp-encoded inner `Process`
///   `Message` (see the module docs). `200` with `{"accepted":true,
///   "tx_id":"..."}` once the pipeline has synchronously accepted it;
///   `400/401/409/422/503` per [`IngressError`].
///
/// Like the health server, the accept loop runs for the process lifetime;
/// draining (a stopped node answering `503`) reuses the shared
/// [`HealthState`], so `mark_stopping()` shuts the front door before the
/// drain sequence begins.
pub fn spawn_ingress_server(
    addr: SocketAddr,
    sink: SubmitSink,
    health: Arc<HealthState>,
    metrics: Arc<Metrics>,
) -> Result<tokio::task::JoinHandle<()>, PneumaticError> {
    // Bind on the caller's thread before any spawn: a port collision must be
    // a synchronous error, never a server that "booted" and listens nowhere
    // (the same rule as the RNS interface and the health bind).
    let std_listener = std::net::TcpListener::bind(addr)
        .map_err(|e| PneumaticError::Network(format!("ingress server bind {addr}: {e}")))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| PneumaticError::Network(format!("ingress listener: {e}")))?;
    let listener = TcpListener::from_std(std_listener)
        .map_err(|e| PneumaticError::Network(format!("ingress listener: {e}")))?;
    tracing::info!(%addr, path = INGRESS_PATH, "transaction ingress listening");
    Ok(spawn_accept_loop(listener, sink, health, metrics))
}

/// Start the ingress on an already-bound listener (production callers use
/// [`spawn_ingress_server`]; tests pass an ephemeral-port listener they hold,
/// so the port cannot be hijacked between bind and accept). Returns the bound
/// address alongside the accept-loop handle.
pub fn spawn_ingress_server_on(
    listener: TcpListener,
    sink: SubmitSink,
    health: Arc<HealthState>,
    metrics: Arc<Metrics>,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), PneumaticError> {
    let addr = listener
        .local_addr()
        .map_err(|e| PneumaticError::Network(format!("ingress listener address: {e}")))?;
    tracing::info!(%addr, path = INGRESS_PATH, "transaction ingress listening");
    Ok((addr, spawn_accept_loop(listener, sink, health, metrics)))
}

/// Accept loop over an already-bound listener — split out so tests bind an
/// ephemeral port without a drop/rebind race (precedent: `telemetry`).
fn spawn_accept_loop(
    listener: TcpListener,
    sink: SubmitSink,
    health: Arc<HealthState>,
    metrics: Arc<Metrics>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let (s, h, m) = (sink.clone(), health.clone(), metrics.clone());
                    tokio::spawn(async move {
                        serve_connection(stream, s, h, m).await;
                    });
                }
                Err(e) => tracing::debug!(error = %e, "ingress accept error"),
            }
        }
    })
}

/// Write a complete response with `Connection: close` (same shape as the
/// telemetry responder, with the status set this surface needs).
async fn write_response(
    mut stream: TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), std::io::Error> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

/// Serve one HTTP connection: read the head, read the declared body (bounded),
/// serve, close. All malformed-head failures get a status or a silent close —
/// never a panic, never a partial read the handler could mis-interpret.
async fn serve_connection(
    mut stream: TcpStream,
    sink: SubmitSink,
    health: Arc<HealthState>,
    metrics: Arc<Metrics>,
) {
    // --- 1. request head ------------------------------------------------
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 1024];
    let head_ok = loop {
        match tokio::time::timeout(
            std::time::Duration::from_secs(READ_TIMEOUT_SECS),
            stream.read(&mut tmp),
        )
        .await
        {
            Ok(Ok(0)) => break !buf.is_empty(),
            Ok(Ok(n)) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break true;
                }
                if buf.len() > MAX_HEAD_BYTES {
                    let _ = write_response(stream, 414, "text/plain", b"head too large").await;
                    return;
                }
            }
            _ => break false,
        }
    };
    if !head_ok {
        return;
    }

    let head = String::from_utf8_lossy(&buf).into_owned();
    let (request_line, headers) = head.split_once("\r\n").unwrap_or((head.as_str(), ""));
    let mut tokens = request_line.split(' ');
    let (Some(method), Some(path)) = (tokens.next(), tokens.next()) else {
        let _ = write_response(stream, 400, "text/plain", b"malformed request line").await;
        return;
    };

    // --- 2. declared body size, bounded BEFORE reading --------------------
    // Content-Length is parsed (and the body drained) ahead of the method/
    // path/draining gates on purpose: an early return that leaves a
    // client-supplied body unread makes the OS send an RST on close, which
    // discards the response the client has not yet read (a 503/415 becomes an
    // unobservable connection reset). Draining the declared body first lets
    // every within-cap refusal reach the client.
    let declared = header_value(&headers, "content-length").and_then(|v| v.trim().parse::<usize>().ok());
    if declared.map_or(false, |n| n > MAX_INGRESS_BODY_BYTES) {
        metrics.increment("pneumatic_ingress_rejected_total", 1);
        let _ = write_response(stream, 413, "text/plain", b"body exceeds the ingress cap").await;
        return;
    }
    let head_len = head.find("\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
    let mut body = buf[head_len.min(buf.len())..].to_vec();
    if let Some(n) = declared {
        while body.len() < n {
            match tokio::time::timeout(
                std::time::Duration::from_secs(READ_TIMEOUT_SECS),
                stream.read(&mut tmp),
            )
            .await
            {
                Ok(Ok(0)) | Err(_) => break, // client stopped short; gates below report it
                Ok(Ok(rd)) => body.extend_from_slice(&tmp[..rd]),
                Ok(Err(_)) => return,
            }
        }
        body.truncate(n.min(MAX_INGRESS_BODY_BYTES));
    }

    // --- 3. method / path / draining / content-type gates -----------------
    if method != "POST" {
        metrics.increment("pneumatic_ingress_rejected_total", 1);
        let _ = write_response(stream, 405, "text/plain", b"POST only").await;
        return;
    }
    if path != INGRESS_PATH {
        metrics.increment("pneumatic_ingress_rejected_total", 1);
        let _ = write_response(stream, 404, "text/plain", b"not found").await;
        return;
    }
    if health.is_stopping() {
        // Draining fails closed at the front door: never accept work the
        // process is on its way to dropping.
        metrics.increment("pneumatic_ingress_rejected_total", 1);
        let _ = write_response(stream, 503, "text/plain", b"node is draining").await;
        return;
    }
    if !header_value(&headers, "content-type")
        .map(|ct| ct.to_ascii_lowercase().starts_with("application/msgpack"))
        .unwrap_or(false)
    {
        metrics.increment("pneumatic_ingress_rejected_total", 1);
        let _ = write_response(stream, 415, "text/plain", b"content-type must be application/msgpack").await;
        return;
    }
    let declared = match declared {
        Some(n) => n,
        None => {
            metrics.increment("pneumatic_ingress_rejected_total", 1);
            let _ = write_response(stream, 400, "text/plain", b"content-length required").await;
            return;
        }
    };
    if body.len() < declared {
        metrics.increment("pneumatic_ingress_rejected_total", 1);
        let _ = write_response(stream, 400, "text/plain", b"body shorter than content-length").await;
        return;
    }

    // --- 4. decode + authenticate + submit -------------------------------
    let outcome = process_submission(&body, &*sink).await;
    match outcome {
        Ok(tx_id) => {
            metrics.increment("pneumatic_ingress_accepted_total", 1);
            let json = format!("{{\"accepted\":true,\"tx_id\":\"{}\"}}", json_escape(&tx_id));
            let _ = write_response(stream, 200, "application/json", json.as_bytes()).await;
        }
        Err(e) => {
            metrics.increment("pneumatic_ingress_rejected_total", 1);
            tracing::debug!(error = ?e, "ingress submission rejected");
            let _ = write_response(stream, e.status(), "text/plain", e.to_string().as_bytes()).await;
        }
    }
}

/// Case-insensitive header lookup over the head's header lines.
fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Decode, authenticate, and submit the envelope. Kept free of socket work so
/// the security-critical path is unit-testable without a TCP round trip.
async fn process_submission(raw: &Vec<u8>, sink: &(dyn Fn(Message) -> SubmitFuture + Send + Sync)) -> Result<String, IngressError> {
    let message: Message = deserialize_rmp_to(raw)
        .map_err(|e| IngressError::Malformed(format!("undecodable envelope: {e}")))?;
    if message.action != "Process" {
        return Err(IngressError::Malformed(format!(
            "ingress accepts only the \"Process\" action, got {:?}",
            message.action
        )));
    }
    let tx: Transaction = deserialize_rmp_to(&message.body)
        .map_err(|e| IngressError::Malformed(format!("undecodable transaction body: {e}")))?;

    // C3 posture, enforced at the edge: the sender is real, its signature
    // over the canonical bytes verifies, and the envelope is bound to the
    // same key the transaction debits. (The sentinel re-checks exactly these
    // facts downstream — ingress adds a refusal earlier, never a weaker one.)
    if tx.sender.is_empty() {
        return Err(IngressError::Malformed("transaction sender is empty".into()));
    }
    if tx.id.is_empty() {
        return Err(IngressError::Malformed("transaction id is empty".into()));
    }
    // Captured before `message` moves into the sink; the response echoes the
    // id of the AUTHENTICATED transaction, never an unverified field.
    let tx_id = tx.id.clone();
    let verifier = Ed25519Provider::generate();
    if !tx
        .verify_sender_signature()
        .map_err(|e| IngressError::Malformed(format!("sender signature check failed: {e}")))?
    {
        return Err(IngressError::Unauthenticated(
            "sender signature does not verify over the canonical transaction bytes".into(),
        ));
    }
    if message.public_key != tx.sender {
        return Err(IngressError::Unauthenticated(
            "envelope key is not the transaction sender (C3 binding)".into(),
        ));
    }
    let envelope_ok = verifier
        .check_signature(&message.signature, &message.public_key, &message.body)
        .map_err(|e| IngressError::Malformed(format!("envelope signature check failed: {e}")))?;
    if !envelope_ok {
        return Err(IngressError::Unauthenticated(
            "envelope signature does not verify over the body".into(),
        ));
    }

    sink(message).await?;
    Ok(tx_id)
}

/// Minimal JSON string escaping — enough for a tx id inside a hand-serialized
/// response (no serde_json in this hot path, matching the telemetry ethos).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

impl std::fmt::Display for IngressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngressError::Malformed(m) => write!(f, "malformed submission: {m}"),
            IngressError::Unauthenticated(m) => write!(f, "unauthenticated: {m}"),
            IngressError::Rejected(m) => write!(f, "rejected by pipeline: {m}"),
            IngressError::Duplicate(m) => write!(f, "duplicate: {m}"),
            IngressError::Unavailable(m) => write!(f, "unavailable: {m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::serialize_to_bytes_rmp;
    use std::sync::Mutex;

    /// A sender account: deterministic key the tx + envelope are signed with.
    fn sender() -> Ed25519Provider {
        Ed25519Provider::from_seed([0x11u8; 32])
    }

    /// A sender-signed transaction.
    fn signed_tx(account: &Ed25519Provider) -> Transaction {
        let mut tx = Transaction {
            payload: vec![],
            gas_limit: 0,
            id: "tx-ingress-1".to_string(),
            action: "Transfer".into(),
            token_id: vec![1],
            bid: None,
            sequence_number: 1,
            sender: account.public_key().expect("pubkey"),
            receiver: vec![2],
            amount: Some(10),
            timestamp: 0,
            result_hash: vec![],
            sender_signature: vec![],
            result_data: vec![],
        };
        let canonical = tx.canonical_signature_bytes().expect("canonical bytes");
        tx.sender_signature = account.sign_data(&canonical).expect("sender signs");
        tx
    }

    /// The fully-formed inner `Process` envelope a client submits.
    fn envelope(account: &Ed25519Provider, tx: &Transaction) -> Vec<u8> {
        let body = serialize_to_bytes_rmp(tx).expect("tx rmp");
        let message = Message {
            chain_id: "test_env".into(),
            action: "Process".into(),
            signature: account.sign_data(&body).expect("envelope signs"),
            public_key: account.public_key().expect("pubkey"),
            body,
            stake_set: None,
        };
        serialize_to_bytes_rmp(&message).expect("envelope rmp")
    }

    /// A sink that accepts everything (used where only the HTTP status is
    /// asserted and the sink's verdict must not be the variable).
    fn accepting_sink() -> SubmitSink {
        let sink: SubmitSink = Arc::new(|_msg: Message| Box::pin(async { Ok(()) }));
        sink
    }

    /// A sink that answers a fixed refusal.
    fn rejecting_sink(err: IngressError) -> SubmitSink {
        let sink: SubmitSink = Arc::new(move |_msg: Message| {
            let err = IngressError::Unavailable(err.to_string());
            Box::pin(async move { Err(err) })
        });
        sink
    }

    /// Bind an ephemeral port and spawn the server; returns its address.
    fn spawn_test_server(sink: SubmitSink) -> SocketAddr {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = std_listener.local_addr().expect("local addr");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let listener = TcpListener::from_std(std_listener).expect("tokio listener");
        spawn_accept_loop(
            listener,
            sink,
            Arc::new(HealthState::new("ingress-test")),
            Arc::new(Metrics::new()),
        );
        addr
    }

    /// One raw HTTP POST over a real loopback socket.
    async fn http_post(addr: SocketAddr, path: &str, content_type: &str, body: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.expect("write head");
        stream.write_all(body).await.expect("write body");
        stream.flush().await.expect("flush");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read to end");
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("status line");
        (status, text)
    }

    #[tokio::test]
    async fn accepted_submission_answers_200_with_the_tx_id() {
        let account = sender();
        let tx = signed_tx(&account);
        let body = envelope(&account, &tx);

        // A sink that also echoes back through a channel we can read.
        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_in = seen.clone();
        let sink: SubmitSink = Arc::new(move |msg: Message| {
            let id = deserialize_rmp_to::<Transaction>(&msg.body)
                .map(|t| t.id)
                .unwrap_or_default();
            *seen_in.lock().unwrap() = Some(id);
            Box::pin(async { Ok(()) })
        });
        let addr = spawn_test_server(sink);

        let (status, response) = http_post(addr, INGRESS_PATH, "application/msgpack", &body).await;
        assert_eq!(status, 200, "response: {response}");
        assert!(response.contains("\"accepted\":true"), "response: {response}");
        assert!(response.contains("tx-ingress-1"), "the tx id must be echoed: {response}");
        assert_eq!(seen.lock().unwrap().as_deref(), Some("tx-ingress-1"));
    }

    #[tokio::test]
    async fn a_forged_sender_signature_is_401_and_reaches_the_sink_never() {
        let account = sender();
        let mut tx = signed_tx(&account);
        tx.amount = Some(999); // tamper AFTER signing: the signature no longer covers it
        let body = {
            // Rebuild the envelope with the now-stale sender signature kept.
            let body = serialize_to_bytes_rmp(&tx).expect("tampered tx rmp");
            let message = Message {
                chain_id: "test_env".into(),
                action: "Process".into(),
                signature: account.sign_data(&body).expect("envelope still signs"),
                public_key: account.public_key().expect("pubkey"),
                body,
                stake_set: None,
            };
            serialize_to_bytes_rmp(&message).expect("rmp")
        };

        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_in = called.clone();
        let sink: SubmitSink = Arc::new(move |_msg| {
            called_in.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        });
        let addr = spawn_test_server(sink);

        let (status, response) = http_post(addr, INGRESS_PATH, "application/msgpack", &body).await;
        assert_eq!(status, 401, "response: {response}");
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst), "the sink must never see an unauthenticated submission");
    }

    #[tokio::test]
    async fn an_envelope_signed_by_alien_keys_over_a_victim_tx_is_401() {
        // The C3 binding: envelope key == tx.sender. Submit a validly
        // sender-signed tx inside an envelope signed by a DIFFERENT key.
        let account = sender();
        let alien = Ed25519Provider::from_seed([0x22u8; 32]);
        let tx = signed_tx(&account);
        let body = serialize_to_bytes_rmp(&tx).expect("tx rmp");
        let message = Message {
            chain_id: "test_env".into(),
            action: "Process".into(),
            signature: alien.sign_data(&body).expect("alien signs envelope"),
            public_key: alien.public_key().expect("alien pubkey"), // != tx.sender
            body,
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&message).expect("rmp");
        let addr = spawn_test_server(accepting_sink());

        let (status, response) = http_post(addr, INGRESS_PATH, "application/msgpack", &raw).await;
        assert_eq!(status, 401, "response: {response}");
    }

    #[tokio::test]
    async fn an_envelope_signature_that_fails_under_the_bound_key_is_401() {
        // The envelope-signature check in isolation. The C3 binding (`public_key
        // == tx.sender`) must PASS so control reaches `check_signature`, and the
        // envelope signature must be the ONLY failing check: a validly
        // sender-signed tx, the correct bound key, but an envelope signature that
        // does not verify over the body under that key. This is the path the
        // "alien key" test never reaches (it trips the binding first), so without
        // it a bypassed envelope check would go undetected.
        let account = sender();
        let tx = signed_tx(&account); // sender signature valid over canonical bytes
        let body = serialize_to_bytes_rmp(&tx).expect("tx rmp");
        let message = Message {
            chain_id: "test_env".into(),
            action: "Process".into(),
            // NOT account.sign_data(&body): a signature over the wrong bytes, so
            // it fails `check_signature(sig, account.pubkey, body)`.
            signature: account.sign_data(b"not the body").expect("signature over wrong bytes"),
            public_key: account.public_key().expect("pubkey"), // == tx.sender, binding passes
            body,
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&message).expect("rmp");
        let addr = spawn_test_server(accepting_sink());

        let (status, response) = http_post(addr, INGRESS_PATH, "application/msgpack", &raw).await;
        assert_eq!(status, 401, "a body that fails the envelope check must be refused: {response}");
    }

    #[tokio::test]
    async fn undecodable_bodies_and_foreign_actions_are_400() {
        let addr = spawn_test_server(accepting_sink());
        let (status, _) = http_post(addr, INGRESS_PATH, "application/msgpack", b"not msgpack at all").await;
        assert_eq!(status, 400);

        let foreign = Message {
            chain_id: "test_env".into(),
            action: "Confirm".into(), // a pipeline-internal action, not ingress
            body: vec![],
            signature: vec![],
            public_key: vec![],
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&foreign).expect("rmp");
        let (status, _) = http_post(addr, INGRESS_PATH, "application/msgpack", &raw).await;
        assert_eq!(status, 400, "ingress must only accept the Process envelope");
    }

    #[tokio::test]
    async fn oversized_bodies_wrong_content_type_and_get_are_refused() {
        let addr = spawn_test_server(accepting_sink());

        // 413 from the header ALONE: the body is never written; the refusal
        // must not wait for (or buffer) what the client declared.
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(
                format!(
                    "POST {INGRESS_PATH} HTTP/1.1\r\nHost: x\r\nContent-Type: application/msgpack\r\n\
                     Content-Length: {}\r\n\r\n",
                    MAX_INGRESS_BODY_BYTES + 1
                )
                .as_bytes(),
            )
            .await
            .expect("write head");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        let text = String::from_utf8_lossy(&buf).into_owned();
        assert!(text.contains("413"), "declared oversize body must be refused at the header: {text}");

        let (status, _) = http_post(addr, INGRESS_PATH, "application/json", b"x").await;
        assert_eq!(status, 415);

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /v1/transactions HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        let text = String::from_utf8_lossy(&buf).into_owned();
        assert!(text.contains("405"), "GET must be refused: {text}");
    }

    #[tokio::test]
    async fn a_rejecting_sink_surfaces_its_status() {
        let account = sender();
        let tx = signed_tx(&account);
        let body = envelope(&account, &tx);
        let addr = spawn_test_server(rejecting_sink(IngressError::Unavailable(
            "no sentinel installed".into(),
        )));

        let (status, _) = http_post(addr, INGRESS_PATH, "application/msgpack", &body).await;
        assert_eq!(status, 503, "the sink's refusal must be observable, not a silent 200");
    }

    #[tokio::test]
    async fn draining_refuses_at_the_front_door() {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = std_listener.local_addr().expect("addr");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let listener = TcpListener::from_std(std_listener).expect("tokio listener");
        let health = Arc::new(HealthState::new("ingress-test"));
        health.mark_stopping();
        spawn_accept_loop(listener, accepting_sink(), health, Arc::new(Metrics::new()));

        let account = sender();
        let body = envelope(&account, &signed_tx(&account));
        let (status, _) = http_post(addr, INGRESS_PATH, "application/msgpack", &body).await;
        assert_eq!(status, 503, "a draining node must not accept new work");
    }

    #[test]
    fn json_escape_protects_the_tx_id_field() {
        assert_eq!(json_escape("plain"), "plain");
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }
}
