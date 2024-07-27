use std::io::{Read, Write};
use std::net::TcpStream;
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use crate::conns::ConnError;

pub trait Stream : Send + Sync {
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, std::io::Error>;
    fn write_all(&mut self, data: &Vec<u8>) -> Result<(), std::io::Error>;
    fn into_split(self: Box<Self>) -> Result<(Box<dyn StreamReader>, Box<dyn StreamWriter>), ConnError>;
}

pub(crate) struct CoreTcpStream {
    inner_stream: TcpStream
}

impl CoreTcpStream {
    pub(crate) fn from_stream(stream: TcpStream) -> Self {
        CoreTcpStream {
            inner_stream: stream
        }
    }
}

impl Stream for CoreTcpStream {
    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> Result<usize, std::io::Error> {
        self.inner_stream.read_to_end(buffer)
    }

    fn write_all(&mut self, data: &Vec<u8>) -> Result<(), std::io::Error> {
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