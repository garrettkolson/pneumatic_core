use std::io::{Error, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use crate::conns::ConnError;

pub trait Stream : Send + Sync {
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, Error>;
    fn read_exact(&mut self, buffer: &mut Vec<u8>) -> Result<(), Error>;
    fn write_all(&mut self, data: &Vec<u8>) -> Result<(), Error>;
    fn into_split(self: Box<Self>) -> Result<(Box<dyn StreamReader>, Box<dyn StreamWriter>), ConnError>;
}

pub struct CoreUdsStream {
    pub inner_stream: UnixStream
}

impl CoreUdsStream {
    pub fn from_stream(stream: UnixStream) -> Self {
        CoreUdsStream {
            inner_stream: stream
        }
    }
}

impl Stream for CoreUdsStream {
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, Error> {
        self.inner_stream.read_to_end(buffer)
    }

    fn read_exact(&mut self, buffer: &mut Vec<u8>) -> Result<(), Error> {
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

pub struct CoreTcpStream {
    pub inner_stream: std::net::TcpStream
}

impl CoreTcpStream {
    pub fn from_stream(stream: TcpStream) -> Self {
        CoreTcpStream {
            inner_stream: stream
        }
    }
}

impl Stream for CoreTcpStream {
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, Error> {
        self.inner_stream.read_to_end(buffer)
    }

    fn read_exact(&mut self, buffer: &mut Vec<u8>) -> Result<(), Error> {
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

#[async_trait]
pub trait StreamReader : Send + Sync {
    async fn read_exact(&mut self, buffer: &mut Vec<u8>) -> Result<usize, ConnError>;
}

pub struct UdsReader {
    pub inner_reader: tokio::net::unix::OwnedReadHalf
}

impl UdsReader {
    pub fn from_owned_read_half(reader: tokio::net::unix::OwnedReadHalf) -> Self {
        UdsReader {
            inner_reader: reader
        }
    }
}

#[async_trait]
impl StreamReader for UdsReader {
    async fn read_exact(&mut self, mut buffer: &mut Vec<u8>) -> Result<usize, ConnError> {
        match self.inner_reader.read_exact(&mut buffer).await {
            Ok(bytes_read) => Ok(bytes_read),
            Err(err) => Err(ConnError::ReadError(Some(err.to_string())))
        }
    }
}

pub struct TcpReader {
    pub inner_reader: OwnedReadHalf
}

impl TcpReader {
    pub fn from_owned_read_half(reader: OwnedReadHalf) -> Self {
        TcpReader {
            inner_reader: reader
        }
    }
}

#[async_trait]
impl StreamReader for TcpReader {
    async fn read_exact(&mut self, mut buffer: &mut Vec<u8>) -> Result<usize, ConnError>{
        match self.inner_reader.read_exact(&mut buffer).await {
            Ok(bytes_read) => Ok(bytes_read),
            Err(err) => Err(ConnError::ReadError(Some(err.to_string())))
        }
    }
}

#[async_trait]
pub trait StreamWriter : Send + Sync {
    async fn write_all(&mut self, data: &[u8]) -> Result<(), ConnError>;
}

pub struct UdsWriter {
    pub inner_writer: tokio::net::unix::OwnedWriteHalf
}

impl UdsWriter {
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

pub struct TcpWriter {
    pub inner_writer: OwnedWriteHalf
}

impl TcpWriter {
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