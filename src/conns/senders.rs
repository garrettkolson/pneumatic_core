use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;
use crate::conns::ConnError;

const CONN_TIMEOUT_IN_SECS: u64 = 60;
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

pub trait Sender: Send + Sync {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError>;
}

pub(crate) struct UdsSender {
    path: String
}

impl UdsSender {
    pub fn new(target: String) -> Self {
        UdsSender {
            path: target
        }
    }
}

impl Sender for UdsSender {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        let mut stream = match UnixStream::connect(&self.path) {
            Ok(str) => str,
            Err(err) => return Err(ConnError::IO(err.to_string()))
        };

        if let Err(err) = stream.write_all(data) {
            return Err(ConnError::IO(err.to_string()))
        }

        let mut buffer = Vec::new();
        match stream.read_to_end(&mut buffer) {
            Err(err) => Err(ConnError::IO(err.to_string())),
            Ok(_) => Ok(buffer)
        }
    }
}

pub(crate) struct TcpSender {
    path: SocketAddr
}

impl TcpSender {
    pub fn new(addr: SocketAddr) -> Self {
        TcpSender {
            path: addr
        }
    }
}

impl Sender for TcpSender {
    fn get_response(&self, data: &[u8]) -> Result<Vec<u8>, ConnError> {
        let mut stream = match TcpStream::connect_timeout(&self.path, Duration::from_secs(CONN_TIMEOUT_IN_SECS)) {
            Ok(stream) => stream,
            Err(err) => return Err(ConnError::IO(err.to_string()))
        };

        if let Err(err) = stream.write_all(data) {
            return Err(ConnError::IO(err.to_string()))
        }

        let mut buffer = Vec::new();
        match stream.read_to_end(&mut buffer) {
            Err(err) => Err(ConnError::IO(err.to_string())),
            Ok(_) => Ok(buffer)
        }
    }
}
#[cfg(test)]
mod senders_tests {
    use super::*;
    use std::io::Read;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    // Helper function to create a temporary Unix domain socket path
    fn temp_uds_path() -> (tempfile::TempDir, String) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let path_str = path.to_str().unwrap().to_string();
        (dir, path_str)
    }

    // Helper function to run a simple echo server over Unix domain socket
    fn run_uds_echo_server(path: String, ready_tx: mpsc::SyncSender<()>) {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let _ = ready_tx.send(());

        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0; 1024];
            if let Ok(size) = stream.read(&mut buffer) {
                let _ = stream.write_all(&buffer[..size]);
            }
        }
    }

    // Helper function to run a simple echo server over TCP
    fn run_tcp_echo_server(addr: SocketAddr, ready_tx: mpsc::SyncSender<()>) {
        let listener = TcpListener::bind(addr).unwrap();
        let _ = ready_tx.send(());

        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0; 1024];
            if let Ok(size) = stream.read(&mut buffer) {
                let _ = stream.write_all(&buffer[..size]);
            }
        }
    }

    #[test]
    fn test_uds_sender_echo() {
        let (_temp_dir, path) = temp_uds_path();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        
        // Start echo server in a separate thread
        let server_path = path.clone();
        let server_handle = thread::spawn(move || {
            run_uds_echo_server(server_path, ready_tx);
        });

        // Wait for server to be ready
        let _ = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();

        // Test UdsSender
        let sender = UdsSender::new(path);
        let test_data = b"hello, uds";
        
        let response = sender.get_response(test_data).unwrap();
        assert_eq!(response, test_data);

        // Clean up
        drop(server_handle);
    }

    #[test]
    fn test_uds_sender_invalid_path() {
        // Use a non-existent socket path
        let sender = UdsSender::new("/nonexistent/path/to/socket".to_string());
        let result = sender.get_response(b"test");
        assert!(matches!(result, Err(ConnError::IO(_))));
    }

    #[test]
    fn test_tcp_sender_echo() {
        // Use a fixed port to avoid port conflicts
        let addr = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let listener = TcpListener::bind(addr).unwrap();
        let local_addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        
        // Start echo server in a separate thread
        let server_handle = thread::spawn(move || {
            // Notify that the server is ready to accept connections
            let _ = ready_tx.send(());
            
            // Accept a single connection and echo back any data
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0; 1024];
                if let Ok(size) = stream.read(&mut buffer) {
                    // Only write if we actually read something
                    if size > 0 {
                        let _ = stream.write_all(&buffer[..size]);
                    }
                }
            }
        });

        // Wait for server to be ready to accept connections
        let _ = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();

        // Test TcpSender with a small delay to ensure server is ready
        std::thread::sleep(Duration::from_millis(100));
        
        let sender = TcpSender::new(local_addr);
        let test_data = b"hello, tcp";
        
        let response = sender.get_response(test_data).unwrap();
        assert_eq!(response, test_data);

        // Clean up
        drop(server_handle);
    }

    #[test]
    fn test_tcp_sender_connection_timeout() {
        // Use an address that's not listening
        let addr = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let sender = TcpSender::new(addr);
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

        // Wait for server to be ready to accept connections
        let _ = ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();

        // Add a small delay to ensure the server is ready
        std::thread::sleep(Duration::from_millis(100));

        // Test TcpSender with a server that closes the connection
        let sender = TcpSender::new(local_addr);
        let result = sender.get_response(b"test");
        
        // The error could be either during write (BrokenPipe) or read (ConnectionReset)
        // Both are IO errors, so we just check for any IO error
        match result {
            Err(ConnError::IO(_)) => (), // Expected
            other => panic!("Expected IO error, got {:?}", other),
        }
        
        // Clean up
        drop(server_handle);
    }
}
