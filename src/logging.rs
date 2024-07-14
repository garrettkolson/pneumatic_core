use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex};

pub trait Logger {
    fn log(&self, message: String);
}

pub struct FileLogger {
    file_path: String,
    file_access: Arc<Mutex<Box<dyn FnMut(&String, String)>>>
}

impl FileLogger {
    fn new(&self, file_name: String) -> Self {
        FileLogger {
            file_path: file_name,
            file_access: Arc::new(Mutex::new(Box::new(|path, mes| {
                Self::log_message(path, mes)
            })))
        }
    }

    fn log_message(path: &str, message: String) {
        let Ok(mut file) = File::options()
            .write(true)
            .append(true)
            .create(true)
            .open(path) else { return };
        let _ = file.write_all(message.as_bytes());
    }
}

impl Logger for FileLogger {
    fn log(&self, message: String) {
        let Ok(mut func) = self.file_access.lock() else { return };
        func(&self.file_path, message)
    }
}