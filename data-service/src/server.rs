//! The data service's wire face: framed MsgPack in, framed MsgPack out.
//!
//! # The frame (must match `pneumatic_core::conns::senders` byte for byte)
//!
//! ```text
//! [4-byte big-endian length][32-byte auth tag][body]
//! ```
//!
//! `length` counts the tag **and** the body. The tag is
//! `HMAC-SHA256(secret, body)`, or 32 zero bytes when no secret is configured
//! (the legacy/dev framing the binaries warn about). Both halves come from
//! `conns::uds::{sign_payload, verify_payload}` — the same helpers the client's
//! `Sender` uses — so authentication cannot drift between the two sides.
//!
//! # One request per connection
//!
//! `Sender::get_response` connects, writes one frame, reads one frame, and
//! drops the stream — there is no keep-alive in the client. So the service
//! handles exactly one request per accepted connection and closes it. That
//! makes thread-per-connection correct here and avoids a session lifetime, at
//! the cost of a socket per call (the client's existing shape; callers are boot
//! loads, the stake refresher, and per-message lookups — not a tight loop).
//!
//! # Fail-closed on garbage
//!
//! A request that cannot be authenticated, deserialized, or framed gets an
//! empty body back. The client then fails to deserialize the response and
//! surfaces a `DataError`, which its callers treat as a hard boot failure or a
//! routing miss. The service never invents a value.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use pneumatic_core::conns::uds::{sign_payload, verify_payload};
use pneumatic_core::conns::MAX_FRAME_SIZE;
use pneumatic_core::data::{DataOp, DataRequest, GetOp, SaveOp};
use pneumatic_core::encoding::{deserialize_rmp_to, serialize_to_bytes_rmp};

use crate::store::DataStore;

/// Length of the auth tag prefixing every frame body.
///
/// Mirrors the client's private `AUTH_TAG_LEN` (`conns/senders.rs`): the HMAC
/// runs over the body only, so the tag is a fixed 32 bytes either way.
const AUTH_TAG_LEN: usize = 32;

/// Read one frame body (tag + payload) from `stream`.
///
/// Returns the raw body *including* the tag, or `None` on a clean EOF before a
/// header — a peer that opened and closed the socket, which is not an error
/// worth logging once per testnet node.
fn read_body(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(header) as usize;
    if len < AUTH_TAG_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame body {len} shorter than the {AUTH_TAG_LEN}-byte auth tag"),
        ));
    }
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame body {len} exceeds the {MAX_FRAME_SIZE}-byte cap"),
        ));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Write one frame (auth tag + body) to `stream`.
fn write_body(stream: &mut TcpStream, secret: Option<&[u8]>, body: &[u8]) -> io::Result<()> {
    let (tag, body) = sign_payload(secret, body);
    let mut frame = Vec::with_capacity(4 + AUTH_TAG_LEN + body.len());
    // The length covers tag + body, so the client's len-then-read round-trips
    // the full authenticated body.
    frame.extend_from_slice(&((AUTH_TAG_LEN + body.len()) as u32).to_be_bytes());
    frame.extend_from_slice(&tag);
    frame.extend_from_slice(&body);
    stream.write_all(&frame)
}

/// Service a `Get`: return the stored bytes verbatim, or an empty body when the
/// key is absent (the protocol has no not-found signal — see module docs).
fn handle_get(store: &DataStore, req: &DataRequest) -> Vec<u8> {
    // Every `GetOp` variant is a plain lookup here: the client already encoded
    // *which* entity it wants into the key (big-endian epoch for stake/executor
    // snapshots, `b"shielded_pool"` for the pool), so the store stays a
    // partition-scoped byte map. The match makes the op explicit at the
    // boundary so a future variant with real dispatch semantics is a compile
    // error here rather than a silent wrong-answer.
    match req.op() {
        DataOp::Get(get) => match get {
            GetOp::Token
            | GetOp::Data
            | GetOp::User
            | GetOp::StakeSnapshot(_)
            | GetOp::ExecutorSet(_)
            | GetOp::ShieldedPool => store.get(req.partition_id(), req.key()).unwrap_or_default(),
        },
        // Routed away by `handle_frame`; empty is the fail-closed answer if
        // that ever changes.
        DataOp::Save(_) => Vec::new(),
    }
}

/// Service a `Save`: store the operation's inner value, re-serialized with the
/// workspace codec so a later `Get` hands the client exactly what it expects to
/// deserialize. Returns an empty body (the client ignores a save's response).
fn handle_save(store: &DataStore, req: &DataRequest) -> io::Result<Vec<u8>> {
    let DataOp::Save(save) = req.op() else {
        return Ok(Vec::new());
    };
    // The key is client-computed and authoritative; the stored value is
    // whatever the corresponding `Get` must hand back — hence one serialization
    // arm per variant, and never the `SaveOp` wrapper (the client deserializes
    // the inner type, e.g. `T = StakeSnapshotEnvelope`).
    let value = match save {
        SaveOp::Token(token) => serialize_to_bytes_rmp(token),
        SaveOp::Data(data) => serialize_to_bytes_rmp(data),
        SaveOp::User(user) => serialize_to_bytes_rmp(user),
        SaveOp::StakeSnapshot(envelope) => serialize_to_bytes_rmp(envelope),
        SaveOp::ExecutorSet(envelope) => serialize_to_bytes_rmp(envelope),
        SaveOp::ShieldedPool(envelope) => serialize_to_bytes_rmp(envelope),
    }
    .map_err(|e| io::Error::other(format!("re-serializing save payload: {e}")))?;

    store
        .put(req.partition_id(), req.key(), &value)
        .map_err(|e| io::Error::other(format!("store put failed: {e}")))?;
    Ok(Vec::new())
}

/// Deserialize and dispatch one authenticated frame payload.
///
/// Returns the response body (untagged); an empty body is the "no answer"
/// signal described in the module docs.
fn dispatch(store: &DataStore, payload: &Vec<u8>) -> Vec<u8> {
    let Ok(req) = deserialize_rmp_to::<DataRequest>(payload) else {
        eprintln!(
            "[data-service] dropping undecodable data request ({} payload bytes)",
            payload.len()
        );
        return Vec::new();
    };

    match req.op() {
        DataOp::Get(_) => handle_get(store, &req),
        DataOp::Save(_) => match handle_save(store, &req) {
            Ok(body) => body,
            Err(e) => {
                // A failed save must not answer as though it persisted: the
                // node would carry on with an unpersisted stake snapshot or
                // pool state and lose it at the next boot. The client's save
                // path treats any reply as success, so the honest signal here
                // is no reply at all (it surfaces as a data error).
                eprintln!("[data-service] save failed: {e}");
                Vec::new()
            }
        },
    }
}

/// Serve exactly one request on an accepted connection, then close it.
fn serve_connection(mut stream: TcpStream, store: Arc<DataStore>, secret: Option<Vec<u8>>) {
    // Bound the read so a half-open peer cannot pin a thread for the life of
    // the process (the client sets its own read timeout; this is the
    // server-side counterpart).
    if let Err(e) = stream.set_read_timeout(Some(std::time::Duration::from_secs(30))) {
        eprintln!("[data-service] set_read_timeout failed: {e}");
    }

    let body = match read_body(&mut stream) {
        Ok(Some(body)) => body,
        Ok(None) => return, // clean close before a request
        Err(e) => {
            eprintln!("[data-service] malformed frame: {e}");
            return;
        }
    };

    // Authenticate under the configured secret. A mismatch gets no reply: the
    // client surfaces `PeerUnauthenticated`, which the node treats as a data
    // error — it never trusts a payload that failed its tag.
    let (tag, payload) = body.split_at(AUTH_TAG_LEN);
    if !verify_payload(secret.as_deref(), tag, payload) {
        eprintln!("[data-service] rejecting request: HMAC verification failed");
        return;
    }

    let response = dispatch(&store, &payload.to_vec());
    if let Err(e) = write_body(&mut stream, secret.as_deref(), &response) {
        eprintln!("[data-service] response write failed: {e}");
    }
    // The stream drops here — one request per connection (module docs).
}

/// Accept connections forever, one thread per connection.
///
/// Blocking std rather than tokio: the service is a side-car with one request
/// per connection and no async work to multiplex, and staying out of a runtime
/// means a misbehaving node cannot stall the accept loop.
pub fn serve(
    listener: TcpListener,
    store: Arc<DataStore>,
    secret: Option<Vec<u8>>,
) -> io::Result<()> {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let store = store.clone();
                let secret = secret.clone();
                // A per-connection panic must not take the accept loop down
                // with it — every node in the cluster reads its chain state
                // through this process.
                thread::spawn(move || serve_connection(stream, store, secret));
            }
            Err(e) => {
                // A single rejected accept (EMFILE, an aborted handshake) is
                // transient; continue rather than exit the service.
                eprintln!("[data-service] accept failed: {e}");
            }
        }
    }
    Ok(())
}

/// Bind `addr`, spawn the accept loop on a background thread, and return the
/// bound address (an ephemeral `:0` bind resolves to the real port) plus the
/// join handle.
///
/// Returning the bound address is what lets an integration test — and a testnet
/// launcher — point a node at a freshly bound service without reserving a fixed
/// port.
pub fn spawn(
    addr: SocketAddr,
    store: Arc<DataStore>,
    secret: Option<Vec<u8>>,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    let handle = thread::spawn(move || {
        if let Err(e) = serve(listener, store, secret) {
            eprintln!("[data-service] accept loop ended: {e}");
        }
    });
    Ok((bound, handle))
}
