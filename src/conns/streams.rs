//! Sync and async byte-stream abstractions over TCP and Unix domain sockets.
//!
//! Two layers exist because the protocol needs both calling shapes:
//!
//! * [`Stream`] is the *sync* (blocking) trait over `std::net::TcpStream` /
//!   `std::os::unix::net::UnixStream`. Used by handshake- and registration-style
//!   code that reads on a worker thread before a connection is promoted to a
//!   long-lived one.
//! * [`StreamReader`] / [`StreamWriter`] are the *async* (`#[async_trait]`,
//!   Tokio) traits over the owned split halves of the Tokio counterparts. A
//!   long-lived connection holds these so its read loop can live in a spawned
//!   Tokio task (see `TcpConnection::from_stream` in `conns.rs`).
//!
//! The bridge between the layers is [`Stream::into_split`]: it consumes the
//! boxed sync stream, flips the socket to non-blocking, converts it to its
//! Tokio form, and splits it into independent owned read/write halves. It takes
//! `self: Box<Self>` so the trait stays object-safe — callers hold
//! `Box<dyn Stream>` everywhere (e.g. `conns::get_data`).
//!
//! None of these types knows about message framing. The wire format (4-byte
//! big-endian length header + MsgPack payload, read in two `read_exact` steps)
//! is implemented in `conns::get_data` / `conns::get_data_async`; streams only
//! guarantee fill-the-exact-buffer / write-the-whole-buffer semantics. No
//! timeouts are configured here: blocking reads wait indefinitely unless the
//! socket owner set a read timeout on the raw stream first, and the async
//! variants rely on Tokio readiness scheduling, not deadlines.

use std::io::{Error, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use crate::conns::ConnError;

/// Blocking read/write abstraction over a connected socket.
///
/// Object-safe: every method takes `&mut self` except
/// [`into_split`](Stream::into_split), which takes `self: Box<Self>` so
/// `Box<dyn Stream>` callers can consume it. `Send + Sync` so a boxed stream
/// can be moved across threads by connection-handling code (message handlers
/// run on worker threads — see the `ThreadPool` in `server.rs`).
pub trait Stream : Send + Sync {
    /// Read every remaining byte from the peer into `buffer` (which is
    /// appended to, not pre-sized), returning the byte count. Blocks until the
    /// peer closes its write half (EOF) — so this fits the "peer sends its whole
    /// message then closes" pattern, never a persistent connection. A
    /// still-open peer blocks the calling thread forever.
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, Error>;
    /// Fill `buffer` completely; partial reads are looped internally until the
    /// buffer is full. If the peer closes before the buffer is full, the
    /// underlying `read_exact` returns `UnexpectedEof` — EOF is an error here,
    /// never a short-but-successful read. This is the primitive the framing
    /// helpers use for both the 4-byte header and the payload
    /// (`conns::get_data`).
    fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), Error>;
    /// Blocking write of the entire `data` buffer; never short-writes — it
    /// returns `Ok(())` only when every byte reached the socket, otherwise the
    /// first I/O error. Callers that speak the wire protocol write the length
    /// header and payload as two separate `write_all` calls, relying on the
    /// kernel socket buffer to keep the frame contiguous.
    fn write_all(&mut self, data: &Vec<u8>) -> Result<(), Error>;
    /// Convert this sync stream into the async pair used by long-lived
    /// connections: set the socket non-blocking, build the Tokio stream, and
    /// split it into owned read/write halves that can be moved into separate
    /// tasks.
    ///
    /// Fails with `ConnError::IO` if the non-blocking flip or the
    /// `from_std` registration fails. The sync stream is consumed — all
    /// subsequent I/O must go through the returned halves. Dropping the
    /// [`StreamWriter`] half closes the write side; `TcpConnection::drop`
    /// (in `conns.rs`) relies on that to wake the paired read loop.
    fn into_split(self: Box<Self>) -> Result<(Box<dyn StreamReader>, Box<dyn StreamWriter>), ConnError>;
}

/// [`Stream`] implementation over a blocking [`UnixStream`].
///
/// The preferred transport for same-host channels (the data-service link in
/// `data.rs` uses UDS on Unix, falling back to TCP loopback elsewhere). The
/// inner field is public so callers can reach the raw socket for option tuning
/// or `shutdown()` before/after wrapping.
pub struct CoreUdsStream {
    pub inner_stream: UnixStream
}

impl CoreUdsStream {
    /// Wrap an already-connected or already-accepted `UnixStream`. No socket
    /// options are configured here — the caller owns connect/accept and any
    /// timeout or buffer tuning.
    pub fn from_stream(stream: UnixStream) -> Self {
        CoreUdsStream {
            inner_stream: stream
        }
    }
}

// All three read/write methods delegate directly to the std `Read`/`Write`
// impls on `UnixStream`; `into_split` performs the sync->async conversion via
// `tokio::net::UnixStream::from_std` (see the `Stream` trait docs for the
// non-blocking requirement this imposes on the caller's socket).
impl Stream for CoreUdsStream {
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, Error> {
        self.inner_stream.read_to_end(buffer)
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), Error> {
        self.inner_stream.read_exact(buffer)
    }

    fn write_all(&mut self, data: &Vec<u8>) -> Result<(), Error> {
        self.inner_stream.write_all(data)
    }

    fn into_split(self: Box<Self>) -> Result<(Box<dyn StreamReader>, Box<dyn StreamWriter>), ConnError> {
        if self.inner_stream.set_nonblocking(true).is_err() {
            return Err(ConnError::IO("Couldn't set underlying stream to non-blocking mode.".to_string()));
        }

        match tokio::net::UnixStream::from_std(self.inner_stream) {
            Err(err) => Err(ConnError::IO(err.to_string())),
            Ok(stream) => {
                let (reader, writer) = stream.into_split();
                let stream_reader = Box::new(UdsReader::from_owned_read_half(reader));
                let stream_writer = Box::new(UdsWriter::from_owned_write_half(writer));
                Ok((stream_reader, stream_writer))
            }
        }
    }
}

/// [`Stream`] implementation over a blocking [`std::net::TcpStream`].
///
/// Used for inter-host links and as the non-Unix fallback for the data-service
/// channel. Semantics are identical to [`CoreUdsStream`]; the type split exists
/// so each wrapper can perform its own Tokio conversion in
/// [`Stream::into_split`] (`from_std` is type-specific). The inner field is
/// public for raw-socket access (`shutdown`, option tuning).
pub struct CoreTcpStream {
    pub inner_stream: std::net::TcpStream
}

impl CoreTcpStream {
    /// Wrap an already-connected or already-accepted `TcpStream`. No socket
    /// options (`nodelay`, timeouts) are configured here — the caller owns
    /// connect/accept and any tuning.
    pub fn from_stream(stream: TcpStream) -> Self {
        CoreTcpStream {
            inner_stream: stream
        }
    }
}

// All three read/write methods delegate directly to the std `Read`/`Write`
// impls on `TcpStream`; `into_split` performs the sync->async conversion via
// `tokio::net::TcpStream::from_std`.
impl Stream for CoreTcpStream {
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, Error> {
        self.inner_stream.read_to_end(buffer)
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), Error> {
        self.inner_stream.read_exact(buffer)
    }

    fn write_all(&mut self, data: &Vec<u8>) -> Result<(), Error> {
        self.inner_stream.write_all(data)
    }

    fn into_split(self: Box<Self>) -> Result<(Box<dyn StreamReader>, Box<dyn StreamWriter>), ConnError> {
        if self.inner_stream.set_nonblocking(true).is_err() {
            return Err(ConnError::IO("Couldn't set underlying stream to non-blocking mode.".to_string()));
        }

        match tokio::net::TcpStream::from_std(self.inner_stream) {
            Err(err) => Err(ConnError::IO(err.to_string())),
            Ok(stream) => {
                let (reader, writer) = stream.into_split();
                let stream_reader = Box::new(TcpReader::from_owned_read_half(reader));
                let stream_writer = Box::new(TcpWriter::from_owned_write_half(writer));
                Ok((stream_reader, stream_writer))
            }
        }
    }
}

/// Async read half of a split stream (Tokio).
///
/// Implemented by [`UdsReader`] and [`TcpReader`] over the owned read halves
/// produced by [`Stream::into_split`]. Errors are mapped to
/// [`ConnError::ReadError`] so consumers like `conns::get_data_async` never see
/// `std::io::Error`.
#[async_trait]
pub trait StreamReader : Send + Sync {
    /// Fill `buffer` completely, awaiting readiness between socket reads
    /// (Tokio loops the partial reads itself). Returns the number of bytes
    /// read, which on success is `buffer.len()`. If the peer closes before the
    /// buffer is full, Tokio's `read_exact` fails with `UnexpectedEof` and this
    /// surfaces as `ConnError::ReadError` — EOF is an error, never a
    /// short-but-successful read. The `TcpConnection` read loop treats that
    /// error as terminal (its `Err(_) => break` arm in `conns.rs`). No
    /// deadline is applied; a silent peer
    /// parks the future indefinitely.
    async fn read_exact(&mut self, buffer: &mut [u8]) -> Result<usize, ConnError>;
}

/// Async read half over a Unix domain socket: owns the
/// `tokio::net::unix::OwnedReadHalf` from a split [`tokio::net::UnixStream`].
pub struct UdsReader {
    pub inner_reader: tokio::net::unix::OwnedReadHalf
}

impl UdsReader {
    /// Take the read half produced by `tokio::net::UnixStream::into_split`
    /// (normally via [`Stream::into_split`]).
    pub fn from_owned_read_half(reader: tokio::net::unix::OwnedReadHalf) -> Self {
        UdsReader {
            inner_reader: reader
        }
    }
}

#[async_trait]
impl StreamReader for UdsReader {
    async fn read_exact(&mut self, mut buffer: &mut [u8]) -> Result<usize, ConnError> {
        match self.inner_reader.read_exact(&mut buffer).await {
            Ok(bytes_read) => Ok(bytes_read),
            Err(err) => Err(ConnError::ReadError(Some(err.to_string())))
        }
    }
}

/// Async read half over TCP: owns the `OwnedReadHalf` from a split
/// [`tokio::net::TcpStream`]. The counterpart of [`UdsReader`] for inter-host
/// links.
pub struct TcpReader {
    pub inner_reader: OwnedReadHalf
}

impl TcpReader {
    /// Take the read half produced by `tokio::net::TcpStream::into_split`
    /// (normally via [`Stream::into_split`]).
    pub fn from_owned_read_half(reader: OwnedReadHalf) -> Self {
        TcpReader {
            inner_reader: reader
        }
    }
}

#[async_trait]
impl StreamReader for TcpReader {
    async fn read_exact(&mut self, mut buffer: &mut [u8]) -> Result<usize, ConnError>{
        match self.inner_reader.read_exact(&mut buffer).await {
            Ok(bytes_read) => Ok(bytes_read),
            Err(err) => Err(ConnError::ReadError(Some(err.to_string())))
        }
    }
}

/// Async write half of a split stream (Tokio).
///
/// Implemented by [`UdsWriter`] and [`TcpWriter`] over the owned write halves
/// from [`Stream::into_split`]. Errors map to [`ConnError::WriteError`].
#[async_trait]
pub trait StreamWriter : Send + Sync {
    /// Await until the entire `data` buffer has been handed to the socket;
    /// Tokio retries short writes internally, so success means every byte was
    /// accepted. Framing-aware callers write the 4-byte length header and the
    /// payload as two consecutive `write_all` calls, serialized by a mutex
    /// around the writer (see `TcpConnection::send` in `conns.rs`) so frames
    /// from concurrent senders never interleave.
    ///
    /// **Dropping the implementor closes the write half of the socket** —
    /// `TcpConnection::drop` uses exactly this to make the paired read loop hit
    /// EOF and terminate.
    async fn write_all(&mut self, data: &[u8]) -> Result<(), ConnError>;
}

/// Async write half over a Unix domain socket: owns the
/// `tokio::net::unix::OwnedWriteHalf` from a split [`tokio::net::UnixStream`].
pub struct UdsWriter {
    pub inner_writer: tokio::net::unix::OwnedWriteHalf
}

impl UdsWriter {
    /// Take the write half produced by `tokio::net::UnixStream::into_split`
    /// (normally via [`Stream::into_split`]).
    pub fn from_owned_write_half(writer: tokio::net::unix::OwnedWriteHalf) -> Self {
        UdsWriter {
            inner_writer: writer
        }
    }
}

#[async_trait]
impl StreamWriter for UdsWriter {
    async fn write_all(&mut self, data: &[u8]) -> Result<(), ConnError> {
        match self.inner_writer.write_all(&data).await {
            Err(err) => Err(ConnError::WriteError(Some(err.to_string()))),
            Ok(()) => Ok(())
        }
    }
}

/// Async write half over TCP: owns the `OwnedWriteHalf` from a split
/// [`tokio::net::TcpStream`]. The counterpart of [`UdsWriter`] for inter-host
/// links.
pub struct TcpWriter {
    pub inner_writer: OwnedWriteHalf
}

impl TcpWriter {
    /// Take the write half produced by `tokio::net::TcpStream::into_split`
    /// (normally via [`Stream::into_split`]).
    pub fn from_owned_write_half(writer: OwnedWriteHalf) -> Self {
        TcpWriter {
            inner_writer: writer
        }
    }
}

#[async_trait]
impl StreamWriter for TcpWriter {
    async fn write_all(&mut self, data: &[u8]) -> Result<(), ConnError> {
        match self.inner_writer.write_all(&data).await {
            Err(err) => Err(ConnError::WriteError(Some(err.to_string()))),
            Ok(()) => Ok(())
        }
    }
}

#[cfg(test)]
pub mod streams_tests {
    // Tests for src/conns/streams.rs
    // This file tests CoreUdsStream, CoreTcpStream, Stream trait, StreamReader, StreamWriter, UdsReader, TcpReader, UdsWriter, TcpWriter
    // All failures should be resolved by fixing the tests, not src/conns/streams.rs

    use std::net::{TcpListener, TcpStream as StdTcpStream};
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::thread;
    use tokio::runtime::Runtime;
    use crate::conns::streams::{CoreUdsStream, CoreTcpStream, UdsReader, TcpReader, UdsWriter, TcpWriter, Stream, StreamReader, StreamWriter};

    // Helper to create a pair of connected UnixStreams
    fn unix_stream_pair() -> (StdUnixStream, StdUnixStream) {
        StdUnixStream::pair().unwrap()
    }

    // Helper to create a pair of connected TcpStreams
    fn tcp_stream_pair() -> (StdTcpStream, StdTcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = thread::spawn(move || StdTcpStream::connect(addr).unwrap());
        let (server, _) = listener.accept().unwrap();
        let client = t.join().unwrap();
        (server, client)
    }

    // Helper to create a Tokio runtime for async tests
    fn create_runtime() -> Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap()
    }

    #[test]
    fn test_core_uds_stream_read_write() {
        let (s1, s2) = unix_stream_pair();
        let mut stream1 = CoreUdsStream::from_stream(s1);
        let mut stream2 = CoreUdsStream::from_stream(s2);
        let msg = b"hello uds".to_vec();
        stream1.write_all(&msg).unwrap();
        let mut buf = vec![0; msg.len()];
        stream2.read_exact(&mut buf).unwrap();
        assert_eq!(buf, msg);
    }

    #[test]
    fn test_core_tcp_stream_read_write() {
        let (s1, s2) = tcp_stream_pair();
        let mut stream1 = CoreTcpStream::from_stream(s1);
        let mut stream2 = CoreTcpStream::from_stream(s2);
        let msg = b"hello tcp".to_vec();
        stream1.write_all(&msg).unwrap();
        let mut buf = vec![0; msg.len()];
        stream2.read_exact(&mut buf).unwrap();
        assert_eq!(buf, msg);
    }

    #[test]
    fn test_core_uds_stream_read_to_end() {
        let (s1, s2) = unix_stream_pair();
        let mut stream1 = CoreUdsStream::from_stream(s1);
        let mut stream2 = CoreUdsStream::from_stream(s2);
        let msg = b"read to end uds".to_vec();
        stream1.write_all(&msg).unwrap();
        stream1.write_all(&Vec::new()).unwrap();
        stream1.inner_stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = Vec::new();
        stream2.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, msg);
    }

    #[test]
    fn test_core_tcp_stream_read_to_end() {
        let (s1, s2) = tcp_stream_pair();
        let mut stream1 = CoreTcpStream::from_stream(s1);
        let mut stream2 = CoreTcpStream::from_stream(s2);
        let msg = b"read to end tcp".to_vec();
        stream1.write_all(&msg).unwrap();
        stream1.inner_stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = Vec::new();
        stream2.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, msg);
    }

    #[test]
    fn test_core_uds_stream_into_split() {
        let rt = create_runtime();
        rt.block_on(async {
            let (s1, _s2) = unix_stream_pair();
            let stream1 = CoreUdsStream::from_stream(s1);
            let boxed = Box::new(stream1);
            let result = boxed.into_split();
            assert!(result.is_ok());
        });
    }

    #[test]
    fn test_core_tcp_stream_into_split() {
        let rt = create_runtime();
        rt.block_on(async {
            let (s1, _s2) = tcp_stream_pair();
            let stream1 = CoreTcpStream::from_stream(s1);
            let boxed = Box::new(stream1);
            let result = boxed.into_split();
            assert!(result.is_ok());
        });
    }

    #[test]
    fn test_uds_reader_writer() {
        let rt = create_runtime();
        rt.block_on(async {
            let (s1, s2) = unix_stream_pair();
            // Set non-blocking mode before converting to Tokio stream
            s1.set_nonblocking(true).unwrap();
            s2.set_nonblocking(true).unwrap();
            
            let tokio_stream1 = tokio::net::UnixStream::from_std(s1).unwrap();
            let tokio_stream2 = tokio::net::UnixStream::from_std(s2).unwrap();
            
            let (r1, w1) = tokio_stream1.into_split();
            let (r2, _w2) = tokio_stream2.into_split();
            
            let mut reader = UdsReader::from_owned_read_half(r2);
            let mut writer = UdsWriter::from_owned_write_half(w1);
            
            let msg = b"uds async rw".to_vec();
            writer.write_all(&msg).await.unwrap();
            
            let mut buf = vec![0; msg.len()];
            let n = reader.read_exact(&mut buf).await.unwrap();
            assert_eq!(n, msg.len());
            assert_eq!(buf, msg);
        });
    }

    #[test]
    fn test_tcp_reader_writer() {
        let rt = create_runtime();
        rt.block_on(async {
            let (s1, s2) = tcp_stream_pair();
            // Set non-blocking mode before converting to Tokio stream
            s1.set_nonblocking(true).unwrap();
            s2.set_nonblocking(true).unwrap();
            
            let tokio_stream1 = tokio::net::TcpStream::from_std(s1).unwrap();
            let tokio_stream2 = tokio::net::TcpStream::from_std(s2).unwrap();
            
            let (r1, w1) = tokio_stream1.into_split();
            let (r2, _w2) = tokio_stream2.into_split();
            
            let mut reader = TcpReader::from_owned_read_half(r2);
            let mut writer = TcpWriter::from_owned_write_half(w1);
            
            let msg = b"tcp async rw".to_vec();
            writer.write_all(&msg).await.unwrap();
            
            let mut buf = vec![0; msg.len()];
            let n = reader.read_exact(&mut buf).await.unwrap();
            assert_eq!(n, msg.len());
            assert_eq!(buf, msg);
        });
    }

}