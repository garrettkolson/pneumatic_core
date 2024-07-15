use std::io::Write;
use file_lock::{FileLock, FileOptions};

pub trait Logger {
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
        let log_entry = format!("{0}: {1}", chrono::Utc::now(), message);
        let _ = file_lock.file.write_all(log_entry.as_bytes());
    }
}