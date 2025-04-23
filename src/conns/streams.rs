use std::io::{Error, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
//use tokio::net::{TcpStream, UnixStream};
use crate::conns::ConnError;

pub trait Stream : Send + Sync {
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, Error>;
    fn read_exact(&mut self, buffer: &mut Vec<u8>) -> Result<(), Error>;
    fn write_all(&mut self, data: &Vec<u8>) -> Result<(), Error>;
    fn into_split(self: Box<Self>) -> Result<(Box<dyn StreamReader>, Box<dyn StreamWriter>), ConnError>;
}

pub(crate) struct CoreUdsStream {
    inner_stream: UnixStream
}

impl CoreUdsStream {
    pub(crate) fn from_stream(stream: UnixStream) -> Self {
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

pub(crate) struct CoreTcpStream {
    inner_stream: std::net::TcpStream
}

impl CoreTcpStream {
    pub(crate) fn from_stream(stream: TcpStream) -> Self {
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

struct UdsReader {
    inner_reader: tokio::net::unix::OwnedReadHalf
}

impl UdsReader {
    fn from_owned_read_half(reader: tokio::net::unix::OwnedReadHalf) -> Self {
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

struct TcpReader {
    inner_reader: OwnedReadHalf
}

impl TcpReader {
    fn from_owned_read_half(reader: OwnedReadHalf) -> Self {
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

struct UdsWriter {
    inner_writer: tokio::net::unix::OwnedWriteHalf
}

impl UdsWriter {
    fn from_owned_write_half(writer: tokio::net::unix::OwnedWriteHalf) -> Self {
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

struct TcpWriter {
    inner_writer: OwnedWriteHalf
}

impl TcpWriter {
    fn from_owned_write_half(writer: OwnedWriteHalf) -> Self {
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