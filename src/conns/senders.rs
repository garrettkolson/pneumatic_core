use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::time::Duration;

use crate::conns::uds::{sign_payload, verify_payload};
use crate::conns::{ConnError, MAX_FRAME_SIZE};
use crate::rns::wrapper::RnsNetwork;

/// How long a connect may take before we give up and fail closed.
const CONNECT_TIMEOUT_SECS: u64 = 5;
/// Length of the HMAC tag prepended to every framed body.
const AUTH_TAG_LEN: usize = 32;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

pub trait Sender: Send + Sync {
    /// Send `data` to the remote and return the response body. The wire form is
    /// `[4-byte BE len][auth_tag(32) || body]`; the tag is a no-op when no
    /// shared secret is configured (backward-compatible / test path).
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError>;
}

/// Map a blocking I/O error onto a `ConnError`, converting a socket timeout
/// (WouldBlock/TimedOut, produced by `set_read_timeout`/`set_write_timeout`)
/// into `Timeout` so a hung peer degrades to an error rather than a hang.
fn map_io_err(e: std::io::Error, kind: IoErrorKind) -> ConnError {
    // A blocking read/write bounded by `set_read_timeout`/`set_write_timeout`
    // returns WouldBlock/TimedOut on expiry — surface that as `Timeout` so a
    // hung peer degrades to an error, not a hang. Any other failure is a plain
    // read/write error in the direction that failed.
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            ConnError::Timeout(e.to_string())
        }
        _ => match kind {
            IoErrorKind::Read => ConnError::ReadError(Some(e.to_string())),
            IoErrorKind::Write => ConnError::WriteError(Some(e.to_string())),
        },
    }
}

#[derive(Clone, Copy)]
enum IoErrorKind {
    Read,
    Write,
}

// --- UDS sender -----------------------------------------------------------

pub(crate) struct UdsSender {
    path: String,
    secret: Option<Vec<u8>>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl UdsSender {
    pub fn new(path: String, secret: Option<Vec<u8>>) -> Self {
        UdsSender {
            path,
            secret,
            read_timeout: None,
            write_timeout: None,
        }
    }

    pub(crate) fn with_secret(self, secret: Option<Vec<u8>>) -> Self {
        UdsSender { secret, ..self }
    }

    /// Bound blocking reads/writes. `None` leaves the existing (unset) default.
    pub(crate) fn with_timeouts(self, read: Option<Duration>, write: Option<Duration>) -> Self {
        UdsSender { read_timeout: read, write_timeout: write, ..self }
    }
}

impl Sender for UdsSender {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        let mut stream = match UnixStream::connect(&self.path) {
            Ok(stream) => stream,
            Err(err) => return Err(ConnError::IO(err.to_string())),
        };
        set_unix_timeouts(&mut stream, self.read_timeout, self.write_timeout);

        write_frame(&mut stream, self.secret.as_deref(), data)?;

        let mut header = [0u8; 4];
        stream.read_exact(&mut header).map_err(|e| map_io_err(e, IoErrorKind::Read))?;
        let data_length = u32::from_be_bytes(header) as usize;
        if data_length > MAX_FRAME_SIZE {
            return Err(ConnError::MalformedData(format!(
                "Frame size {} exceeds maximum {}", data_length, MAX_FRAME_SIZE
            )));
        }
        let mut body = vec![0u8; data_length];
        stream.read_exact(&mut body).map_err(|e| map_io_err(e, IoErrorKind::Read))?;
        unframe(self.secret.as_deref(), &body)
    }
}

// --- TCP sender -----------------------------------------------------------

pub(crate) struct TcpSender {
    path: SocketAddr,
    secret: Option<Vec<u8>>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl TcpSender {
    pub fn new(addr: SocketAddr, secret: Option<Vec<u8>>) -> Self {
        TcpSender {
            path: addr,
            secret,
            read_timeout: None,
            write_timeout: None,
        }
    }

    pub(crate) fn with_secret(self, secret: Option<Vec<u8>>) -> Self {
        TcpSender { secret, ..self }
    }

    pub(crate) fn with_timeouts(self, read: Option<Duration>, write: Option<Duration>) -> Self {
        TcpSender { read_timeout: read, write_timeout: write, ..self }
    }
}

impl Sender for TcpSender {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        let mut stream = match TcpStream::connect_timeout(&self.path, Duration::from_secs(CONNECT_TIMEOUT_SECS)) {
            Ok(stream) => stream,
            Err(err) => return Err(ConnError::IO(err.to_string())),
        };
        set_tcp_timeouts(&mut stream, self.read_timeout, self.write_timeout);

        write_frame(&mut stream, self.secret.as_deref(), data)?;

        let mut header = [0u8; 4];
        stream.read_exact(&mut header).map_err(|e| map_io_err(e, IoErrorKind::Read))?;
        let data_length = u32::from_be_bytes(header) as usize;
        if data_length > MAX_FRAME_SIZE {
            return Err(ConnError::MalformedData(format!(
                "Frame size {} exceeds maximum {}", data_length, MAX_FRAME_SIZE
            )));
        }
        let mut body = vec![0u8; data_length];
        stream.read_exact(&mut body).map_err(|e| map_io_err(e, IoErrorKind::Read))?;
        unframe(self.secret.as_deref(), &body)
    }
}

// --- RNS sender (async, transport-agnostic) -------------------------------

pub struct RnsSender {
    network: Arc<RnsNetwork>,
    rhash: [u8; 16],
}

impl RnsSender {
    pub fn new(network: Arc<RnsNetwork>, rhash: [u8; 16]) -> Self {
        RnsSender { network, rhash }
    }
}

impl Sender for RnsSender {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        // Send payload through RNS; no response expected (async delivery)
        self.network
            .send_to(self.rhash, data)
            .map_err(|e| ConnError::IO(e.to_string()))?;
        Ok(vec![])
    }
}

// --- shared framing + auth helpers ----------------------------------------

/// Set read/write timeouts on a UDS so a hung peer degrades to a `Timeout`
/// error instead of blocking the caller thread forever.
fn set_unix_timeouts(
    stream: &mut UnixStream,
    read: Option<Duration>,
    write: Option<Duration>,
) {
    if let Some(d) = read {
        let _ = stream.set_read_timeout(Some(d));
    }
    if let Some(d) = write {
        let _ = stream.set_write_timeout(Some(d));
    }
}

/// Set read/write timeouts on a TCP socket (same contract as the UDS path).
fn set_tcp_timeouts(
    stream: &mut TcpStream,
    read: Option<Duration>,
    write: Option<Duration>,
) {
    if let Some(d) = read {
        let _ = stream.set_read_timeout(Some(d));
    }
    if let Some(d) = write {
        let _ = stream.set_write_timeout(Some(d));
    }
}

/// Write one framed, optionally-authenticated request: `[len][auth_tag || body]`.
/// The 4-byte length covers `auth_tag || body`, so `get_data`'s cap covers the
/// whole body.
fn write_frame<W: Write>(stream: &mut W, secret: Option<&[u8]>, payload: &[u8]) -> Result<(), ConnError> {
    let (tag, body) = sign_payload(secret, payload);
    let mut frame = Vec::with_capacity(4 + AUTH_TAG_LEN + payload.len());
    // The length counts the tag *and* the body, so the reader's `len`-then-read
    // round-trips the full authenticated body (not just the bare payload).
    frame.extend_from_slice(&((AUTH_TAG_LEN + body.len()) as u32).to_be_bytes());
    frame.extend_from_slice(&tag);
    frame.extend_from_slice(&body);
    stream
        .write_all(&frame)
        .map_err(|e| map_io_err(e, IoErrorKind::Write))
}

/// Split a read body into `(auth_tag, payload)` and verify the tag. The 16 MB
/// cap is enforced by the caller before this runs.
fn unframe(secret: Option<&[u8]>, body: &[u8]) -> Result<Vec<u8>, ConnError> {
    if body.len() < AUTH_TAG_LEN {
        return Err(ConnError::MalformedData(
            "response shorter than auth tag".into(),
        ));
    }
    let (tag, payload) = body.split_at(AUTH_TAG_LEN);
    if verify_payload(secret, tag, payload) {
        Ok(payload.to_vec())
    } else {
        Err(ConnError::Unauthenticated(
            "response HMAC verification failed".into(),
        ))
    }
}

#[cfg(test)]
mod senders_tests {
    use super::*;
    use std::io::Read;
    use std::sync::mpsc;
    use std::thread;
    use tempfile::tempdir;

    // Helper function to create a temporary Unix domain socket path
    fn temp_uds_path() -> (tempfile::TempDir, String) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let path_str = path.to_str().unwrap().to_string();
        (dir, path_str)
    }

    /// Echo back a request body's *payload* (the bytes after its 32-byte auth
    /// tag), re-wrapped in a fresh framed + authed response. Returns `None` if
    /// the body is too short to contain a tag (malformed / no secret).
    fn echo_response(secret: Option<&[u8]>, body: &[u8]) -> Option<Vec<u8>> {
        if body.len() < AUTH_TAG_LEN {
            return None;
        }
        let payload = &body[AUTH_TAG_LEN..];
        let (tag, echoed) = sign_payload(secret, payload);
        let mut out = Vec::with_capacity(4 + AUTH_TAG_LEN + payload.len());
        out.extend_from_slice(&((AUTH_TAG_LEN + echoed.len()) as u32).to_be_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&echoed);
        Some(out)
    }

    /// UDS echo server that speaks the new framed + authenticated wire format.
    /// Accepts one connection, reads one framed request, and echoes back the
    /// request payload in a framed, re-signed response.
    fn run_uds_echo_server(path: String, secret: Option<Vec<u8>>, ready_tx: mpsc::SyncSender<()>) {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let _ = ready_tx.send(());

        if let Ok((mut stream, _)) = listener.accept() {
            let mut header = [0u8; 4];
            if stream.read_exact(&mut header).is_ok() {
                let len = u32::from_be_bytes(header) as usize;
                if len > MAX_FRAME_SIZE {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    return;
                }
                let mut body = vec![0u8; len];
                if stream.read_exact(&mut body).is_ok() {
                    if let Some(out) = echo_response(secret.as_deref(), &body) {
                        let _ = stream.write_all(&out);
                    }
                }
            }
        }
    }

    /// TCP echo server speaking the framed + authenticated wire format. Binds
    /// `addr` (which may be port `0` for an ephemeral port) and reports the
    /// actual bound address over `ready_tx` so the test can connect to it.
    fn run_tcp_echo_server(addr: SocketAddr, secret: Option<Vec<u8>>, ready_tx: mpsc::SyncSender<SocketAddr>) {
        let listener = TcpListener::bind(addr).unwrap();
        let local_addr = listener.local_addr().unwrap();
        let _ = ready_tx.send(local_addr);

        if let Ok((mut stream, _)) = listener.accept() {
            let mut header = [0u8; 4];
            if stream.read_exact(&mut header).is_ok() {
                let len = u32::from_be_bytes(header) as usize;
                if len > MAX_FRAME_SIZE {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    return;
                }
                let mut body = vec![0u8; len];
                if stream.read_exact(&mut body).is_ok() {
                    if let Some(out) = echo_response(secret.as_deref(), &body) {
                        let _ = stream.write_all(&out);
                    }
                }
            }
        }
    }

    #[test]
    fn test_uds_sender_echo() {
        let (_temp_dir, path) = temp_uds_path();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let server_path = path.clone();
        let server_handle = thread::spawn(move || {
            run_uds_echo_server(server_path, None, ready_tx);
        });

        let _ = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        thread::sleep(Duration::from_millis(100));

        let sender = UdsSender::new(path, None);
        let test_data = b"hello, uds";

        let response = sender.get_response(test_data).unwrap();
        assert_eq!(response, test_data);

        drop(server_handle);
    }

    #[test]
    fn test_uds_sender_echo_with_secret() {
        let (_temp_dir, path) = temp_uds_path();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let secret = vec![9, 8, 7, 6, 5, 4, 3, 2, 1, 0];

        let server_path = path.clone();
        let server_secret = secret.clone();
        let server_handle = thread::spawn(move || {
            run_uds_echo_server(server_path, Some(server_secret), ready_tx);
        });

        let _ = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        thread::sleep(Duration::from_millis(100));

        // Correct secret round-trips.
        let sender = UdsSender::new(path, Some(secret));
        let test_data = b"authed uds";
        let response = sender.get_response(test_data).unwrap();
        assert_eq!(response, test_data);

        drop(server_handle);
    }

    // Discriminator (ground rule 2): a wrong secret is rejected. Reverting the
    // verify in `unframe` makes this return Ok, failing the assert.
    #[test]
    fn test_uds_sender_wrong_secret_rejected() {
        let (_temp_dir, path) = temp_uds_path();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let secret = vec![1u8, 2, 3, 4, 5, 6];
        let wrong = vec![9u8, 9, 9, 9, 9, 9];

        let server_path = path.clone();
        let server_handle = thread::spawn(move || {
            // Speak framing, but authenticate with the WRONG secret.
            let _ = std::fs::remove_file(&server_path);
            let listener = UnixListener::bind(&server_path).unwrap();
            let _ = ready_tx.send(());
            if let Ok((mut stream, _)) = listener.accept() {
                let mut header = [0u8; 4];
                let _ = stream.read_exact(&mut header);
                let len = u32::from_be_bytes(header) as usize;
                let mut body = vec![0u8; len];
                let _ = stream.read_exact(&mut body);
                // Echo the payload but re-sign it under the WRONG secret, so the
                // client's verify with the correct secret fails.
                if let Some(out) = echo_response(Some(&wrong), &body) {
                    let _ = stream.write_all(&out);
                }
            }
        });

        let _ = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        thread::sleep(Duration::from_millis(100));

        let sender = UdsSender::new(path, Some(secret));
        let result = sender.get_response(b"hello");
        assert!(
            matches!(result, Err(ConnError::Unauthenticated(_))),
            "expected Unauthenticated, got {:?}",
            result
        );

        drop(server_handle);
    }

    // Discriminator (verify a): a server that accepts and reads the request but
    // never responds must make `get_response` return `Timeout`, not hang.
    #[test]
    fn test_uds_sender_read_timeout() {
        let (_temp_dir, path) = temp_uds_path();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let server_path = path.clone();
        let server_handle = thread::spawn(move || {
            let _ = std::fs::remove_file(&server_path);
            let listener = UnixListener::bind(&server_path).unwrap();
            let _ = ready_tx.send(());
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the request so the client's write completes, then hold
                // the connection open WITHOUT responding.
                let mut header = [0u8; 4];
                if stream.read_exact(&mut header).is_ok() {
                    let len = u32::from_be_bytes(header) as usize;
                    let mut body = vec![0u8; len.min(MAX_FRAME_SIZE + 1)];
                    let _ = stream.read_exact(&mut body);
                }
                std::thread::sleep(Duration::from_secs(4));
            }
        });

        let _ = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        thread::sleep(Duration::from_millis(100));

        let sender = UdsSender::new(path, None)
            .with_timeouts(Some(Duration::from_secs(1)), Some(Duration::from_secs(1)));
        let result = sender.get_response(b"hello");
        assert!(
            matches!(result, Err(ConnError::Timeout(_))),
            "expected Timeout within the read bound, got {:?}",
            result
        );

        drop(server_handle);
    }

    // Discriminator (verify c): a response whose length header exceeds
    // MAX_FRAME_SIZE is rejected with MalformedData before any allocation.
    #[test]
    fn test_uds_sender_rejects_oversized() {
        let (_temp_dir, path) = temp_uds_path();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let server_path = path.clone();
        let server_handle = thread::spawn(move || {
            let _ = std::fs::remove_file(&server_path);
            let listener = UnixListener::bind(&server_path).unwrap();
            let _ = ready_tx.send(());
            if let Ok((mut stream, _)) = listener.accept() {
                // Write only a bogus length header; never send the body.
                let bogus: u32 = (MAX_FRAME_SIZE as u32) + 1;
                let _ = stream.write_all(&bogus.to_be_bytes());
                std::thread::sleep(Duration::from_secs(2));
            }
        });

        let _ = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        thread::sleep(Duration::from_millis(100));

        let sender = UdsSender::new(path, None);
        let result = sender.get_response(b"x");
        assert!(
            matches!(result, Err(ConnError::MalformedData(_))),
            "expected MalformedData for oversized frame, got {:?}",
            result
        );

        drop(server_handle);
    }

    #[test]
    fn test_uds_sender_invalid_path() {
        // Use a non-existent socket path
        let sender = UdsSender::new("/nonexistent/path/to/socket".to_string(), None);
        let result = sender.get_response(b"test");
        assert!(matches!(result, Err(ConnError::IO(_))));
    }

    #[test]
    fn test_tcp_sender_echo() {
        // Bind port 0; the server reports its actual bound address on ready_tx.
        let addr = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let server_handle = thread::spawn(move || {
            run_tcp_echo_server(addr, None, ready_tx);
        });

        // Wait for server to be ready, and learn its actual bound port.
        let actual_addr = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();

        // Test TcpSender with a small delay to ensure server is ready
        std::thread::sleep(Duration::from_millis(100));

        let sender = TcpSender::new(actual_addr, None);
        let test_data = b"hello, tcp";

        let response = sender.get_response(test_data).unwrap();
        assert_eq!(response, test_data);

        drop(server_handle);
    }

    #[test]
    fn test_tcp_sender_echo_with_secret() {
        let addr = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let secret = vec![42u8; 16];

        let server_secret = secret.clone();
        let server_handle = thread::spawn(move || {
            run_tcp_echo_server(addr, Some(server_secret), ready_tx);
        });

        let actual_addr = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let sender = TcpSender::new(actual_addr, Some(secret));
        let test_data = b"authed tcp";

        let response = sender.get_response(test_data).unwrap();
        assert_eq!(response, test_data);

        drop(server_handle);
    }

    #[test]
    fn test_tcp_sender_connection_timeout() {
        // Use an address that's not listening
        let addr = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let sender = TcpSender::new(addr, None);
        let result = sender.get_response(b"test");
        assert!(matches!(result, Err(ConnError::IO(_))));
    }

    #[test]
    fn test_tcp_sender_write_error() {
        // Create a server that accepts connections but immediately closes them
        let addr = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let listener = TcpListener::bind(addr).unwrap();
        let local_addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        // Start server in a separate thread
        let server_handle = thread::spawn(move || {
            // Notify that the server is ready to accept connections
            let _ = ready_tx.send(());

            // Accept a connection and immediately close it
            if let Ok((stream, _)) = listener.accept() {
                // Close the connection immediately
                drop(stream);
            }
        });

        // Wait for server to be ready
        let _ = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();

        // Add a small delay to ensure the server is ready
        std::thread::sleep(Duration::from_millis(100));

        // Test TcpSender with a server that closes the connection
        let sender = TcpSender::new(local_addr, None);
        let result = sender.get_response(b"test");

        // Framed read over a closed socket yields a read/EOF error (or, under a
        // timeout, a Timeout) — either way an Err, never a panic.
        assert!(result.is_err());

        // Clean up
        drop(server_handle);
    }

    #[test]
    fn test_tcp_sender_read_timeout() {
        let addr = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let listener = TcpListener::bind(addr).unwrap();
        let local_addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let server_handle = thread::spawn(move || {
            let _ = ready_tx.send(());
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the request, then hold open without responding.
                let mut header = [0u8; 4];
                let _ = stream.read_exact(&mut header);
                std::thread::sleep(Duration::from_secs(4));
                let _ = stream;
            }
        });

        let _ = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let sender = TcpSender::new(local_addr, None)
            .with_timeouts(Some(Duration::from_secs(1)), None);
        let result = sender.get_response(b"hello");
        assert!(
            matches!(result, Err(ConnError::Timeout(_))),
            "expected Timeout, got {:?}",
            result
        );

        drop(server_handle);
    }
}
