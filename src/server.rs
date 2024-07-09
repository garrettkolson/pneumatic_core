use std::sync::{Arc, mpsc, Mutex};
use std::sync::mpsc::SendError;
use std::thread;

pub struct ThreadPool {
    workers: Vec<Worker>,
    sender: Option<mpsc::Sender<Job>>
}

impl ThreadPool {

    /// Creates a new ThreadPool with the specified number of threads.
    ///
    /// # Panics
    ///
    /// This function will panic if the size is less than 1.
    pub fn new(size: usize) -> ThreadPool {
        assert!(size > 0);
        ThreadPool::generate_thread_pool(size)
    }

    /// Creates a new ThreadPool with specified number of threads,
    /// when size is greater than 0. If size is 0, a PoolCreationError will be returned.
    pub fn build(size: usize) -> Result<ThreadPool, PoolCreationError> {
        match size {
            0 => Err(PoolCreationError {
                message: "The size of the thread pool cannot be 0".to_string()
            }),
            _ => Ok(ThreadPool::generate_thread_pool(size))
        }
    }

    fn generate_thread_pool(size: usize) -> ThreadPool {
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(size);
        for i in 0..size {
            workers.push(Worker::new(i, Arc::clone(&receiver)));
        }

        ThreadPool {
            workers,
            sender: Some(sender)
        }
    }

    pub fn execute<F>(&self, func: F) -> Result<(), SendError<Job>>
        where F : FnOnce() + Send + 'static {
        let job = Box::new(func);
        let sender_as_ref = match self.sender.as_ref() {
            Some(r) => r,
            None => return Ok(())
        };

        sender_as_ref.send(job)
    }
}

struct Worker {
    id: usize,
    thread: Option<thread::JoinHandle<Result<(), WorkerError>>>
}

impl Worker {
    fn new(id: usize, receiver: Arc<Mutex<mpsc::Receiver<Job>>>)
           -> Worker {
        let thread = thread::spawn(move || loop {
            let mutex = match receiver.lock() {
                Ok(l) => l,
                Err(err) => return Err(WorkerError::ReceiverPoisoned(err.to_string()))
            };

            return match mutex.recv() {
                Err(err) => Err(WorkerError::WhileReceiving(err.to_string())),
                Ok(job) => {
                    println!("Worker {0} got a job... executing.", id);
                    job();
                    Ok(())
                }
            };
        });

        Worker {
            id,
            thread: Some(thread)
        }
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug)]
pub struct PoolCreationError {
    pub message: String
}

#[derive(Debug)]
pub enum WorkerError {
    ReceiverPoisoned(String),
    WhileReceiving(String)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc, Mutex};
    use std::thread;
    use crate::server::{Job, ThreadPool, Worker};

    #[test]
    #[should_panic]
    fn calling_thread_pool_new_with_zero_threads_panics() {
        let pool = ThreadPool::new(0);
    }

    #[test]
    fn calling_thread_pool_new_with_more_than_zero_returns_thread_pool_with_correct_number_of_workers() {
        let size = 300;
        let pool = ThreadPool::new(size);
        assert_eq!(pool.workers.len(), size)
    }

    #[test]
    fn calling_thread_pool_build_with_zero_threads_returns_pool_creation_error() {
        let pool = ThreadPool::build(0);
        assert!(pool.is_err_and(|err| err.message == "The size of the thread pool cannot be 0"))
    }

    #[test]
    fn calling_thread_pool_build_with_more_than_zero_threads_returns_ok() {
        let size = 54;
        let pool = ThreadPool::build(size);
        assert!(pool.is_ok_and(|result| result.workers.len() == size));
    }

    #[test]
    fn calling_worker_new_returns_new_worker() {
        let id = 23;
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        let worker = Worker::new(id, receiver);
        assert_eq!(worker.id, id)
    }

    #[test]
    fn calling_thread_pool_execute_should_run_the_closure() {
        let mut stuff: &'static str = "stuff";
        let pool = ThreadPool::new(23);
        let _ = pool.execute(move || {
            stuff = "more stuff";
            assert_eq!(stuff, "more stuff");
        });
    }

    #[test]
    fn calling_thread_pool_execute_with_poisoned_mutex_should_not_run_the_closure() {
        let id = 23;
        let (sender, receiver) = mpsc::channel::<Job>();
        let mutex = Arc::new(Mutex::new(receiver));

        let cloned_mutex = Arc::clone(&mutex);
        let _ = thread::spawn(move || {
            let mut data = cloned_mutex.lock().unwrap();
            panic!();
        }).join();

        let worker = Worker::new(id, Arc::clone(&mutex));
        let mut pool = ThreadPool::new(1);
        pool.workers[0] = worker;

        let _ = pool.execute(|| {
            panic!("This should not actually panic");
        });
    }
}