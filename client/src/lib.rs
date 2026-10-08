//! Pneumatic transaction client (rollout roadmap Phase 1, ADR-020).
//!
//! Builds a [`Transaction`], signs it exactly as a real sender must, wraps it
//! in the inner `Process` envelope, and POSTs it to a node's transaction
//! ingress ([`pneumatic_core::ingress`]). There is no client-side protocol:
//! the bytes a client submits are the same inner envelope a submitting peer
//! relays on the RNS wire, so a transaction accepted from this client and one
//! accepted from a peer are indistinguishable to the node from the dispatcher
//! down.
//!
//! Scope is submission only. Acceptance is not commitment — after a `200`,
//! the chain of custody passes to the data service, which is where a
//! committed block is observed (`DataProvider::get_token`).

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

use pneumatic_core::crypto::{AsymCryptoProvider, Ed25519Provider};
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};
use pneumatic_core::ingress::INGRESS_PATH;
use pneumatic_core::messages::Message;
use pneumatic_core::transactions::Transaction;

/// Connect/response timeout for one submission round trip. A node's ingress
/// itself answers synchronously (accept/refuse); this bound is purely the
/// socket, so a hung peer cannot hang the client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything that can keep a submission from being cleanly accepted.
#[derive(Debug)]
pub enum ClientError {
    /// Socket-level failure (connect, write, read, timeout).
    Transport(String),
    /// The node answered with a non-200 status and its honest reason as the
    /// body — `400` malformed, `401` unauthenticated, `409` duplicate,
    /// `422` pipeline refusal, `503` unavailable/draining.
    Rejected { status: u16, reason: String },
    /// A 2xx whose body was not the documented acceptance JSON.
    BadResponse(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Transport(m) => write!(f, "transport failure: {m}"),
            ClientError::Rejected { status, reason } => write!(f, "node refused submission ({status}): {reason}"),
            ClientError::BadResponse(m) => write!(f, "malformed node response: {m}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Proof of ACCEPTANCE (not commitment — see module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitReceipt {
    /// The transaction id, echoed back by the node from the AUTHENTICATED
    /// transaction it accepted.
    pub tx_id: String,
}

/// A submitting identity bound to one ingress endpoint.
///
/// The account is shared-state by design: the CLI drives many submissions
/// (incrementing sequence numbers) through one client, and the key material
/// never moves.
pub struct TxClient {
    addr: SocketAddr,
    chain_id: String,
    account: Arc<Ed25519Provider>,
}

impl TxClient {
    pub fn new(addr: SocketAddr, chain_id: impl Into<String>, account: Arc<Ed25519Provider>) -> Self {
        Self { addr, chain_id: chain_id.into(), account }
    }

    /// The public key submissions will be attributed to (C3: this key must
    /// own the transaction's `sender`).
    pub fn public_key(&self) -> Result<Vec<u8>, ClientError> {
        self.account.public_key().map_err(|e| ClientError::Transport(format!("public key: {e}")))
    }

    /// Sign `tx` in place (fills `sender` + `sender_signature`) and submit it.
    ///
    /// The caller sets everything the account does not own: id, token, action,
    /// receiver, amount, nonce, payload. Signing happens HERE and only here,
    /// so the bytes the client sends are byte-identical to the bytes signed —
    /// there is no re-serialization step that could drift.
    pub async fn submit(&self, tx: &mut Transaction) -> Result<SubmitReceipt, ClientError> {
        tx.sender = self.public_key()?;
        let canonical = tx
            .canonical_signature_bytes()
            .map_err(|e| ClientError::Transport(format!("canonical bytes: {e}")))?;
        tx.sender_signature = self
            .account
            .sign_data(&canonical)
            .map_err(|e| ClientError::Transport(format!("sender signature: {e}")))?;

        // The inner `Process` envelope, exactly as a relaying peer's gossiper
        // produces it: body = rmp(Transaction), signature = the sender's hybrid
        // signature over that body, public_key = the sender.
        let body = serialize_to_bytes_rmp(tx)
            .map_err(|e| ClientError::Transport(format!("tx encoding: {e}")))?;
        let envelope_signature = self
            .account
            .sign_data(&body)
            .map_err(|e| ClientError::Transport(format!("envelope signature: {e}")))?;
        let envelope = Message {
            chain_id: self.chain_id.clone(),
            action: "Process".to_string(),
            signature: envelope_signature,
            public_key: tx.sender.clone(),
            body,
            stake_set: None,
        };
        let raw = serialize_to_bytes_rmp(&envelope)
            .map_err(|e| ClientError::Transport(format!("envelope encoding: {e}")))?;

        let (status, resp_body) = self.post(&raw).await?;
        if status != 200 {
            return Err(ClientError::Rejected { status, reason: resp_body });
        }
        // The ingress answers 200 with `{"accepted":true,"tx_id":"..."}`.
        // Verify the JSON rather than trusting the status alone: a 200 that
        // does not affirm this tx is a broken server, not a success.
        let v =
            serde_json_lite::parse(&resp_body).map_err(|e| ClientError::BadResponse(format!("{e}: {resp_body}")))?;
        if v.get("accepted") != Some(&serde_json_lite::Value::Bool(true)) {
            return Err(ClientError::BadResponse(format!("accepted!=true: {resp_body}")));
        }
        let echoed = v
            .get("tx_id")
            .and_then(|t| t.as_str())
            .ok_or_else(|| ClientError::BadResponse(format!("missing tx_id: {resp_body}")))?;
        if echoed != tx.id {
            return Err(ClientError::BadResponse(format!(
                "node echoed a different tx_id: {echoed:?} != {:?}",
                tx.id
            )));
        }
        Ok(SubmitReceipt { tx_id: tx.id.clone() })
    }

    /// Convenience for the plain-value transfer shape: fills the transfer
    /// fields and delegates to [`submit`](Self::submit).
    pub async fn submit_transfer(
        &self,
        tx_id: impl Into<String>,
        token_id: &[u8],
        receiver: Vec<u8>,
        amount: u64,
        sequence_number: usize,
    ) -> Result<SubmitReceipt, ClientError> {
        let mut tx = Transaction {
            id: tx_id.into(),
            action: "Transfer".to_string(),
            token_id: token_id.to_vec(),
            bid: None,
            sequence_number,
            sender: Vec::new(),
            receiver,
            amount: Some(amount),
            timestamp: 0,
            result_hash: Vec::new(),
            sender_signature: Vec::new(),
            payload: Vec::new(),
            gas_limit: 0,
            result_data: Vec::new(),
        };
        self.submit(&mut tx).await
    }

    /// Minimal HTTP POST over a fresh connection (the mirror of the ingress
    /// responder: read the status line and body to `Connection: close` EOF —
    /// no keep-alive to mismanage).
    async fn post(&self, body: &[u8]) -> Result<(u16, String), ClientError> {
        let connect = |addr: SocketAddr| async move {
            TcpStream::connect(addr).await.map_err(|e| ClientError::Transport(format!("connect {addr}: {e}")))
        };
        let mut stream = timeout(REQUEST_TIMEOUT, connect(self.addr))
            .await
            .map_err(|_| ClientError::Transport(format!("connect {addr} timed out", addr = self.addr)))??;
        let request = format!(
            "POST {INGRESS_PATH} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Content-Type: application/msgpack\r\n\
             Content-Length: {len}\r\n\
             \r\n",
            host = self.addr,
            len = body.len()
        );
        let head = request.as_bytes();
        let raw = timeout(REQUEST_TIMEOUT, async {
            stream.write_all(head).await.map_err(|e| ClientError::Transport(format!("write head: {e}")))?;
            stream.write_all(body).await.map_err(|e| ClientError::Transport(format!("write body: {e}")))?;
            stream.flush().await.map_err(|e| ClientError::Transport(format!("flush: {e}")))?;
            // The responder closes after its answer, so read-to-EOF is a
            // complete response — status line plus body.
            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).await.map_err(|e| ClientError::Transport(format!("read response: {e}")))?;
            Ok::<_, ClientError>(raw)
        })
        .await
        .map_err(|_| ClientError::Transport("response timed out".into()))??;

        let text = String::from_utf8_lossy(&raw).into_owned();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ClientError::BadResponse(format!("no status line: {text:?}")))?;
        let body_text = match text.find("\r\n\r\n") {
            Some(i) => text[i + 4..].to_string(),
            None => String::new(),
        };
        Ok((status, body_text))
    }
}

// ---------------------------------------------------------------------------
// A purpose-built slice of JSON for exactly one response shape
// ---------------------------------------------------------------------------

/// The ingress response is a two-field flat JSON object. Pulling in
/// `serde_json` for a client whose dependency budget is "tokio + core" is
/// not worth it; this parses flat objects with string/bool values and
/// nothing else, failing loudly on anything richer (which the server never
/// emits).
mod serde_json_lite {
    #[derive(Debug, PartialEq, Eq)]
    pub enum Value {
        String(String),
        Bool(bool),
    }

    pub struct JsonObject(pub Vec<(String, Value)>);

    impl JsonObject {
        pub fn get(&self, key: &str) -> Option<&Value> {
            self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
        }
    }

    impl Value {
        pub fn as_str(&self) -> Option<&str> {
            match self {
                Value::String(s) => Some(s),
                _ => None,
            }
        }
    }

    pub fn parse(text: &str) -> Result<JsonObject, String> {
        let text = text.trim();
        let inner = text
            .strip_prefix('{')
            .and_then(|t| t.strip_suffix('}'))
            .ok_or_else(|| "not a JSON object".to_string())?;
        let mut fields = Vec::new();
        let mut rest = inner.trim();
        if rest.is_empty() {
            return Ok(JsonObject(fields));
        }
        loop {
            rest = rest.trim_start();
            let key_end = rest
                .find(':')
                .ok_or_else(|| format!("expected ':' in {rest:?}"))?;
            let key_raw = rest[..key_end].trim();
            let key = unquote(key_raw).ok_or_else(|| format!("key not a JSON string: {key_raw:?}"))?;
            rest = rest[key_end + 1..].trim_start();
            let value = if rest.starts_with("true") {
                rest = &rest[4..];
                Value::Bool(true)
            } else if rest.starts_with("false") {
                rest = &rest[5..];
                Value::Bool(false)
            } else {
                let end = find_string_end(rest).ok_or_else(|| format!("unterminated string value in {rest:?}"))?;
                let s = unquote(&rest[..end + 1])
                    .ok_or_else(|| format!("value not a JSON string: {:?}", &rest[..end.min(rest.len())]))?;
                rest = &rest[end + 1..];
                Value::String(s)
            };
            fields.push((key, value));
            rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix(',') {
                rest = r;
                continue;
            }
            if rest.is_empty() {
                return Ok(JsonObject(fields));
            }
            return Err(format!("trailing garbage: {rest:?}"));
        }
    }

    fn unquote(raw: &str) -> Option<String> {
        let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
        // Accept (and decode) only the escapes the server's json_escape emits.
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let cp = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(cp)?);
                }
                _ => return None,
            }
        }
        Some(out)
    }

    /// Index just past the closing quote of a JSON string starting at `"`.
    fn find_string_end(s: &str) -> Option<usize> {
        let bytes = s.as_bytes();
        if bytes.first() != Some(&b'"') {
            return None;
        }
        let mut i = 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => return Some(i),
                _ => i += 1,
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_the_ingress_response() {
            let v = parse(r#"{"accepted":true,"tx_id":"tx-1"}"#).expect("parses");
            assert_eq!(v.get("accepted"), Some(&Value::Bool(true)));
            assert_eq!(v.get("tx_id").and_then(Value::as_str), Some("tx-1"));
        }

        #[test]
        fn decodes_escapes_and_rejects_garbage() {
            let v = parse(r#"{"tx_id":"a\"b\n"}"#).expect("parses");
            assert_eq!(v.get("tx_id").and_then(Value::as_str), Some("a\"b\n"));
            assert!(parse("not json").is_err());
            assert!(parse(r#"{"a":1}"#).is_err(), "numbers are outside the documented shape");
            assert!(parse(r#"{"accepted":false,"reason":"x"}"#).is_ok());
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Arc<Ed25519Provider> {
        Arc::new(Ed25519Provider::from_seed([0x11u8; 32]))
    }

    /// A canned-answer HTTP responder: reads one request (head + declared
    /// body, returned as captured bytes) and writes one prepared response.
    /// Stands in for the ingress while the E2E test in node-server covers the
    /// real one. The capture is load-bearing: tests assert against the
    /// request bytes the client actually put on the wire, not a reconstruction.
    async fn stub_server(response: &'static str) -> (SocketAddr, Arc<std::sync::Mutex<Vec<u8>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let captured: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = captured.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            // Read head, then exactly the declared body (so the client never
            // sees a write error — same drain discipline as the real ingress).
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            let head_end = loop {
                let n = stream.read(&mut tmp).await.expect("read request");
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i + 4);
                }
            };
            if let Some(head_len) = head_end {
                let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
                let cl = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim().eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                while buf.len() < head_len + cl {
                    let n = stream.read(&mut tmp).await.expect("read body");
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                *cap.lock().unwrap() = buf;
            }
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        });
        (addr, captured)
    }

    /// Pull the message body out of a captured raw HTTP request.
    fn captured_http_body(raw: &[u8]) -> Vec<u8> {
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("head separator");
        raw[sep + 4..].to_vec()
    }

    #[tokio::test]
    async fn a_signed_submission_puts_valid_envelope_bytes_on_the_wire() {
        // The stub answers 200 with THIS tx's id; what the test then pins is
        // that the captured request bytes ARE a valid inner Process envelope:
        // decoded and both signatures re-verified exactly as the ingress C3
        // gate does. A client whose bytes fail here would be rejected by
        // every real node.
        let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n{\"accepted\":true,\"tx_id\":\"tx-client-1\"}";
        let (addr, captured) = stub_server(ok).await;
        let account = account();
        let client = TxClient::new(addr, "test_env", account.clone());

        let receipt = client
            .submit_transfer("tx-client-1", &[1], vec![9], 5, 7)
            .await
            .expect("stub accepted");
        assert_eq!(receipt.tx_id, "tx-client-1");

        let raw = captured.lock().unwrap().clone();
        let head = String::from_utf8_lossy(&raw).into_owned();
        assert!(head.starts_with("POST /v1/transactions HTTP/1.1\r\n"), "request line: {head}");
        assert!(
            head.to_ascii_lowercase().contains("content-type: application/msgpack"),
            "content-type must be msgpack: {head}"
        );

        let envelope: Message = deserialize_rmp_to(&captured_http_body(&raw)).expect("wire bytes are a Message");
        assert_eq!(envelope.action, "Process");
        assert_eq!(envelope.chain_id, "test_env");
        let tx: Transaction = deserialize_rmp_to(&envelope.body).expect("body is an rmp Transaction");
        assert_eq!(tx.id, "tx-client-1");
        assert_eq!(tx.sequence_number, 7);
        assert_eq!(tx.sender, account.public_key().expect("pubkey"), "sender bound to the account key");
        assert!(tx.verify_sender_signature().expect("verify"), "wire tx passes the C3 sender gate");
        assert_eq!(envelope.public_key, tx.sender, "envelope bound to the sender");
        assert!(
            account
                .check_signature(&envelope.signature, &envelope.public_key, &envelope.body)
                .expect("check"),
            "wire envelope signature verifies over the exact body bytes"
        );
    }

    #[tokio::test]
    async fn non_200_surfaces_as_a_rejection_with_the_node_reason() {
        let resp = "HTTP/1.1 409 Conflict\r\nContent-Type: text/plain\r\nContent-Length: 11\r\n\r\nseen before";
        let (addr, _captured) = stub_server(resp).await;
        let client = TxClient::new(addr, "test_env", account());
        let err = client.submit_transfer("tx-dup", &[1], vec![9], 5, 1).await.expect_err("409");
        match err {
            ClientError::Rejected { status, reason } => {
                assert_eq!(status, 409);
                assert_eq!(reason, "seen before");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_200_must_affirm_this_tx_id() {
        // A server that answers 200 but echoes a foreign tx_id is broken, and
        // the client must not report success on its behalf.
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 38\r\n\r\n{\"accepted\":true,\"tx_id\":\"other\"}";
        let (addr, _captured) = stub_server(resp).await;
        let client = TxClient::new(addr, "test_env", account());
        let err = client.submit_transfer("tx-mine", &[1], vec![9], 5, 1).await.expect_err("mismatch");
        assert!(matches!(err, ClientError::BadResponse(_)), "{err}");

        // The well-formed matching answer is the happy path.
        let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 37\r\n\r\n{\"accepted\":true,\"tx_id\":\"tx-mine\"}";
        let (addr, _captured) = stub_server(ok).await;
        let client = TxClient::new(addr, "test_env", account());
        let receipt = client.submit_transfer("tx-mine", &[1], vec![9], 5, 1).await.expect("accepted");
        assert_eq!(receipt.tx_id, "tx-mine");
    }

    #[tokio::test]
    async fn connect_failure_is_a_transport_error_not_a_panic() {
        // Bind then drop to get a port nobody listens on.
        let addr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            l.local_addr().expect("addr")
        };
        let client = TxClient::new(addr, "test_env", account());
        let err = client.submit_transfer("tx-x", &[1], vec![9], 5, 1).await.expect_err("refused");
        assert!(matches!(err, ClientError::Transport(_)), "{err}");
    }
}
