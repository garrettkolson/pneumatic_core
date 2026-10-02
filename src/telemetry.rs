//! Production-readiness telemetry (Phase 8): structured logging init, an
//! in-process metrics registry rendered in Prometheus text format, an HTTP
//! health endpoint, and a shutdown-signal helper.
//!
//! Design notes:
//!
//! * **No HTTP crate.** The health/metrics endpoint is a minimal HTTP/1.1
//!   responder written directly over `tokio::net::TcpListener`. It parses only
//!   the request line of a `GET`, always answers `Connection: close`, and
//!   bounds the request head — the attack surface is the exact shape a
//!   load-balancer healthcheck or a Prometheus scraper produces, nothing more.
//! * **Metrics are lock-free atomics.** A `DashMap` of `AtomicU64` keyed by
//!   metric name; the hot path pays one sharded-map lookup and a relaxed
//!   atomic add. Gauges are `set`, counters are `increment`.
//! * **`Logger` (logging.rs) is untouched.** The consensus-side `Logger` trait
//!   is a durable file-append channel with its own semantics; `tracing` here
//!   is the operator-facing structured stream for binaries and lifecycle
//!   events. Bridging the two is deliberately out of scope.
//! * **Fail-soft boot.** `spawn_health_server` returns a `Result`; a node that
//!   cannot bind its health port logs and continues — health is an ops
//!   affordance, not a consensus dependency (same tolerance as the RNS
//!   transport boot path).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::errors::PneumaticError;

/// Maximum request-head bytes accepted from one client. The head is parsed
/// only to extract the `GET` path; anything beyond this bound is refused.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Socket read timeout per connection, so a slow-loris style client cannot
/// pin a connection task indefinitely.
const READ_TIMEOUT_SECS: u64 = 5;

/// Initialize the operator-facing `tracing` subscriber for a binary.
///
/// Filter precedence: `RUST_LOG` when set, otherwise `info`. Output is
/// line-oriented to stdout with target and timestamp — the format is
/// intentionally plain so container log collectors and `journalctl` can
/// ingest it without a dedicated parser.
///
/// Idempotent for the process: a second call (e.g. from a test that ran
/// before another) leaves the first subscriber installed.
pub fn init_tracing(service: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let installed = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_writer(std::io::stdout)
        .try_init();
    match installed {
        Ok(()) => tracing::info!(service, "tracing initialized"),
        Err(e) => eprintln!("[pneumatic] tracing already initialized: {e}"),
    }
}

/// In-process metrics registry: named `u64` counters and gauges backed by
/// relaxed atomics behind a `DashMap`.
///
/// Names follow Prometheus conventions (`snake_case`, unit suffixes); the
/// renderer emits the Prometheus text exposition format so a stock scraper
/// can ingest `/metrics` without a bridge.
#[derive(Default)]
pub struct Metrics {
    counters: DashMap<String, AtomicU64>,
    gauges: DashMap<String, AtomicU64>,
}

impl Metrics {
    /// A new, empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `delta` to a counter, creating it at 0 first if absent.
    pub fn increment(&self, name: &str, delta: u64) {
        self.counters
            .entry(name.to_string())
            .and_modify(|c| {
                c.fetch_add(delta, Ordering::Relaxed);
            })
            .or_insert_with(|| AtomicU64::new(delta));
    }

    /// Set a gauge to an absolute value (the canonical "read a depth and
    /// publish it" operation for pollers).
    pub fn set_gauge(&self, name: &str, value: u64) {
        self.gauges
            .entry(name.to_string())
            .and_modify(|g| g.store(value, Ordering::Relaxed))
            .or_insert_with(|| AtomicU64::new(value));
    }

    /// Current value of a counter or gauge (`None` when the name is unknown).
    /// Counters are checked first; a name must not be used as both.
    pub fn get(&self, name: &str) -> Option<u64> {
        self.counters
            .get(name)
            .or_else(|| self.gauges.get(name))
            .map(|v| v.load(Ordering::Relaxed))
    }

    /// Render the Prometheus text exposition format. Names are sorted so the
    /// output is stable for tests and diff-friendly for scrapers that hash
    /// scrape samples.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut names: Vec<String> = self
            .counters
            .iter()
            .chain(self.gauges.iter())
            .map(|e| e.key().clone())
            .collect();
        names.sort();
        for name in names {
            if let Some(v) = self.counters.get(&name) {
                out.push_str(&format!("# TYPE {name} counter\n"));
                out.push_str(&format!("{name} {}\n", v.load(Ordering::Relaxed)));
            } else if let Some(v) = self.gauges.get(&name) {
                out.push_str(&format!("# TYPE {name} gauge\n"));
                out.push_str(&format!("{name} {}\n", v.load(Ordering::Relaxed)));
            }
        }
        out
    }
}

/// Liveness/draining state behind the health endpoint.
///
/// A node serves `200` while running and flips to `503` the moment a shutdown
/// signal is received — before any drain work — so load balancers and
/// compose/orchestrator healthchecks stop sending work before the process
/// begins to wind down.
pub struct HealthState {
    service: String,
    version: String,
    stopping: AtomicBool,
}

impl HealthState {
    /// New healthy state for a named service (version from this crate's
    /// `CARGO_PKG_VERSION`).
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            stopping: AtomicBool::new(false),
        }
    }

    /// Flip to draining. Idempotent; call first thing in the shutdown path.
    pub fn mark_stopping(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// Whether shutdown has been initiated.
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// The `/health` JSON body (hand-serialized — three fixed fields, no
    /// need to pull serde_json into the hot path).
    pub fn status_json(&self) -> String {
        let status = if self.is_stopping() { "stopping" } else { "ok" };
        format!(
            "{{\"status\":\"{status}\",\"service\":\"{}\",\"version\":\"{}\"}}",
            self.service, self.version
        )
    }
}

/// Serve one HTTP connection: parse the request line, answer, close.
///
/// All failures are silent closes (a healthcheck that misbehaves is not
/// worth a log line per packet); malformed or oversized heads get 400/414
/// only when the head itself arrived, and slow readers are cut off by
/// [`READ_TIMEOUT_SECS`].
async fn serve_connection(mut stream: TcpStream, health: Arc<HealthState>, metrics: Arc<Metrics>) {
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
            // Read error or timeout: drop the connection without a response.
            _ => break false,
        }
    };
    if !head_ok {
        return;
    }
    // Request line is the first line; path is the second token of a GET.
    let head = String::from_utf8_lossy(&buf);
    let mut lines = head.split("\r\n");
    let Some(request_line) = lines.next() else { return };
    let mut tokens = request_line.split(' ');
    let (Some(method), Some(path)) = (tokens.next(), tokens.next()) else {
        let _ = write_response(stream, 400, "text/plain", b"malformed request line").await;
        return;
    };
    if method != "GET" && method != "HEAD" {
        let _ = write_response(stream, 405, "text/plain", b"method not allowed").await;
        return;
    }
    match path {
        "/health" => {
            let code = if health.is_stopping() { 503 } else { 200 };
            let body = health.status_json();
            let _ = write_response(stream, code, "application/json", body.as_bytes()).await;
        }
        "/metrics" => {
            let body = metrics.render();
            let _ = write_response(
                stream,
                200,
                "text/plain; version=0.0.4",
                body.as_bytes(),
            )
            .await;
        }
        _ => {
            let _ = write_response(stream, 404, "text/plain", b"not found").await;
        }
    }
}

/// Write a complete response with `Connection: close`. The stream is dropped
/// after the flush, closing the socket.
async fn write_response(
    mut stream: TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), std::io::Error> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        414 => "URI Too Long",
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

/// Start the health/metrics HTTP server on `addr`.
///
/// * `GET /health` → `200` JSON while healthy, `503` once
///   [`HealthState::mark_stopping`] has been called.
/// * `GET /metrics` → Prometheus text exposition from [`Metrics::render`].
///
/// The accept loop runs on the current Tokio runtime and never terminates;
/// callers shut down by exiting the process (the endpoint has no in-flight
/// work worth draining).
pub async fn spawn_health_server(
    addr: SocketAddr,
    health: Arc<HealthState>,
    metrics: Arc<Metrics>,
) -> Result<tokio::task::JoinHandle<()>, PneumaticError> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| PneumaticError::Network(format!("health server bind {addr}: {e}")))?;
    tracing::info!(%addr, "health server listening (/health, /metrics)");
    Ok(spawn_accept_loop(listener, health, metrics))
}

/// Accept loop over an already-bound listener — split out so tests can bind
/// an ephemeral port without a drop/rebind race.
fn spawn_accept_loop(
    listener: TcpListener,
    health: Arc<HealthState>,
    metrics: Arc<Metrics>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let (h, m) = (health.clone(), metrics.clone());
                    tokio::spawn(async move {
                        serve_connection(stream, h, m).await;
                    });
                }
                // Per-accept errors are transient (fd limits, resets during
                // the SYN handshake); log at debug and keep serving.
                Err(e) => tracing::debug!(error = %e, "health accept error"),
            }
        }
    })
}

/// Await the first of `SIGINT` (Ctrl-C) and, on Unix, `SIGTERM`.
///
/// SIGTERM is the container orchestrator and `docker stop` convention;
/// ignoring it would guarantee a later `SIGKILL` and an ungraceful exit.
/// Any signal-setup failure falls back to Ctrl-C-only rather than refusing
/// to run.
pub async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        // A failure to listen for Ctrl-C is fatal for THIS future; fall back
        // to sleeping forever so the other signal arm can still fire.
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(unix)]
    {
        let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[pneumatic] cannot listen for SIGTERM ({e}); Ctrl-C only");
                let _ = ctrl_c.await;
                return;
            }
        };
        let term = sigterm.recv();
        tokio::pin!(term);
        tokio::select! {
            _ = ctrl_c => tracing::info!("received SIGINT"),
            _ = &mut term => tracing::info!("received SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await;
        tracing::info!("received Ctrl-C");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// One raw HTTP GET over a real loopback socket; returns
    /// `(status_code, full_response)`.
    async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .expect("write");
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

    /// Bind an ephemeral port and spawn the server, returning its address.
    async fn spawn_test_server(
    ) -> (SocketAddr, Arc<HealthState>, Arc<Metrics>) {
        let health = Arc::new(HealthState::new("test-node"));
        let metrics = Arc::new(Metrics::new());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        spawn_accept_loop(listener, health.clone(), metrics.clone());
        (addr, health, metrics)
    }

    #[test]
    fn metrics_render_sorted_prometheus_text() {
        let m = Metrics::new();
        m.increment("pneumatic_messages_total", 3);
        m.increment("pneumatic_messages_total", 4);
        m.set_gauge("pneumatic_peers", 2);
        m.set_gauge("aa_gauge_first", 1); // sorts before the counter
        let out = m.render();
        assert!(out.contains("# TYPE aa_gauge_first gauge\naa_gauge_first 1\n"));
        assert!(out.contains("# TYPE pneumatic_messages_total counter\npneumatic_messages_total 7\n"));
        assert!(out.contains("# TYPE pneumatic_peers gauge\npneumatic_peers 2\n"));
        // Sorted output: names appear in ascending order.
        let names: Vec<usize> = [
            out.find("aa_gauge_first ").unwrap(),
            out.find("pneumatic_messages_total ").unwrap(),
            out.find("pneumatic_peers ").unwrap(),
        ]
        .to_vec();
        assert!(names.windows(2).all(|w| w[0] < w[1]), "render must sort names");
    }

    #[test]
    fn metrics_get_reads_both_kinds() {
        let m = Metrics::new();
        m.increment("c", 5);
        m.set_gauge("g", 9);
        assert_eq!(m.get("c"), Some(5));
        assert_eq!(m.get("g"), Some(9));
        assert_eq!(m.get("missing"), None);
    }

    #[tokio::test]
    async fn health_endpoint_reports_ok_then_stopping() {
        let (addr, health, _m) = spawn_test_server().await;

        let (status, body) = http_get(addr, "/health").await;
        assert_eq!(status, 200);
        assert!(body.contains("\"status\":\"ok\""));
        assert!(body.contains("\"service\":\"test-node\""));

        health.mark_stopping();
        let (status, body) = http_get(addr, "/health").await;
        assert_eq!(status, 503);
        assert!(body.contains("\"status\":\"stopping\""));
    }

    #[tokio::test]
    async fn metrics_endpoint_serves_prometheus_text() {
        let (addr, _h, m) = spawn_test_server().await;
        m.set_gauge("pneumatic_epoch_current", 7);
        let (status, body) = http_get(addr, "/metrics").await;
        assert_eq!(status, 200);
        assert!(body.contains("text/plain"));
        assert!(body.contains("pneumatic_epoch_current 7"));
    }

    #[tokio::test]
    async fn unknown_paths_and_methods_are_rejected() {
        let (addr, _h, _m) = spawn_test_server().await;
        let (status, _) = http_get(addr, "/nope").await;
        assert_eq!(status, 404);

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"POST /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        assert!(String::from_utf8_lossy(&buf).starts_with("HTTP/1.1 405"));
    }

    #[tokio::test]
    async fn silent_client_is_closed_without_response() {
        // Connect and close without sending a request line: the server must
        // not hang and must not panic.
        let (addr, _h, _m) = spawn_test_server().await;
        let stream = TcpStream::connect(addr).await.expect("connect");
        drop(stream);
        // A subsequent well-formed request still works (server survived).
        let (status, _) = http_get(addr, "/health").await;
        assert_eq!(status, 200);
    }

    #[test]
    fn health_state_json_is_valid_shape() {
        let h = HealthState::new("svc");
        let j = h.status_json();
        let v: serde_json::Value = serde_json::from_str(&j).expect("valid JSON");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["service"], "svc");
        h.mark_stopping();
        let v: serde_json::Value = serde_json::from_str(&h.status_json()).expect("valid JSON");
        assert_eq!(v["status"], "stopping");
    }
}
