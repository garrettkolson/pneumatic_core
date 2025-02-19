use std::io::Write;
use file_lock::{FileLock, FileOptions};

pub trait Logger : Send + Sync {
    fn log(&self, message: String);
}

pub struct FileLogger {
    file_path: String,
}

impl FileLogger {
    pub fn new(file_name: String) -> Self {
        FileLogger {
            file_path: file_name
        }
    }
}

impl Logger for FileLogger {
    fn log(&self, message: String) {
        let options = FileOptions::new().write(true).append(true).create(true);

        let Ok(mut file_lock) = FileLock::lock(&self.file_path, true, options) else { return };
        let log_entry = format!("\r\n{0}: {1}", chrono::Utc::now(), message);
        let _ = file_lock.file.write_all(log_entry.as_bytes());
    }
}

#[cfg(test)]
pub mod tests {
    use std::error::Error;
    use std::io::Read;
    use super::*;
    use crate::logging::FileLogger;

    #[test]
    fn logger_should_log_message_to_file() {
        let logger = FileLogger::new("log.txt".to_string());
        let timestamp = chrono::Utc::now().to_string();

        let str = format!("this is a message {}", timestamp);
        logger.log(str.to_string());

        let result: Result<FileLock, std::io::Error> = match FileLock::lock("log.txt", false, FileOptions::new().read(true)) {
            Ok(file) => Ok(file),
            Err(err) => {
                panic!("error: {0}", err.to_string());
            }
        };

        let mut file = result.unwrap();
        let mut buffer = String::new();
        let Ok(log_text) = file.file.read_to_string(&mut buffer)
            else { panic!() };

        assert!(buffer.contains(&str));
    }
}