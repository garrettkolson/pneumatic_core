use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, mpsc, Mutex};
use std::sync::mpsc::SendError;
use std::thread;
use tokio::sync::mpsc::Receiver;

//const ASYNC_CHANNEL_BUFFER_LEN: usize = 10;

pub struct ThreadPool {
    workers: Vec<Worker>,
    sender: Option<mpsc::Sender<Job>>,
    async_sender: Option<mpsc::Sender<AsyncJob>>
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

        // let (async_sender, async_receiver) = tokio::sync::mpsc::channel::<AsyncJob>(ASYNC_CHANNEL_BUFFER_LEN);
        // let async_receiver = Arc::new(tokio::sync::Mutex::new(async_receiver));
        let (async_sender, async_receiver) = mpsc::channel::<AsyncJob>();
        let async_receiver = Arc::new(tokio::sync::Mutex::new(async_receiver));

        let mut workers = Vec::with_capacity(size);
        for i in 0..size {
            workers.push(Worker::new(i, receiver.clone(), async_receiver.clone()));
        }

        ThreadPool {
            workers,
            sender: Some(sender),
            async_sender: Some(async_sender)
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

    pub fn execute_async<F>(&self, func: F) -> Result<(), SendError<AsyncJob>>
        where F : Future<Output = ()> + Send + Unpin + 'static {
        let job = Box::pin(func);
        let sender = self.async_sender.as_ref().unwrap();
        sender.send(job)
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        drop(self.sender.take());

        for worker in &mut self.workers {
            println!("Shutting down worker {}", worker.id);
            if let Some(thread) = worker.thread.take() {
                let _ = thread.join().unwrap();
            }

            if let Some(async_thread) = worker.async_thread.take() {
                tokio::spawn(async move {
                    async_thread.await
                });
            }
        }
    }
}

struct Worker {
    id: usize,
    thread: Option<thread::JoinHandle<Result<(), WorkerError>>>,
    async_thread: Option<tokio::task::JoinHandle<Result<(), WorkerError>>>
}

impl Worker {
    fn new(id: usize,
                 receiver: Arc<Mutex<mpsc::Receiver<Job>>>,
                 async_recv: Arc<tokio::sync::Mutex<mpsc::Receiver<AsyncJob>>>)
           -> Worker {
        Worker {
            id,
            thread: Some(Self::get_sync_thread(receiver)),
            async_thread: Some(Self::get_async_thread(async_recv))
        }
    }

    fn get_sync_thread(receiver: Arc<Mutex<mpsc::Receiver<Job>>>)
                        -> thread::JoinHandle<Result<(), WorkerError>> {
        thread::spawn(move || loop {
            let mutex = match receiver.lock() {
                Ok(l) => l,
                Err(err) => return Err(WorkerError::ReceiverPoisoned(err.to_string()))
            };

            match mutex.recv() {
                Err(_) => return Ok(()),
                Ok(job) => {
                    job();
                }
            };
        })
    }

    fn get_async_thread(receiver: Arc<tokio::sync::Mutex<mpsc::Receiver<AsyncJob>>>)
                        -> tokio::task::JoinHandle<Result<(), WorkerError>> {
        tokio::task::spawn(async move {
            loop {
                let mutex = receiver.lock().await;
                match mutex.recv() {
                    Err(_) => return Ok(()),
                    Ok(job) => {
                        let _ = Box::pin(job.await);
                    }
                };
            }
        })
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;
type AsyncJob = Pin<Box<dyn Future<Output = ()> + Send + Unpin + 'static>>;

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
    use crate::server::{AsyncJob, Job, ThreadPool, Worker};

    #[tokio::test]
    #[should_panic]
    async fn calling_thread_pool_new_with_zero_threads_panics() {
        let pool = ThreadPool::new(0);
    }

    #[tokio::test]
    async fn calling_thread_pool_new_with_more_than_zero_returns_thread_pool_with_correct_number_of_workers() {
        let size = 300;
        let pool = ThreadPool::new(size);
        assert_eq!(pool.workers.len(), size)
    }

    #[tokio::test]
    async fn calling_thread_pool_build_with_zero_threads_returns_pool_creation_error() {
        let pool = ThreadPool::build(0);
        assert!(pool.is_err_and(|err| err.message == "The size of the thread pool cannot be 0"))
    }

    #[tokio::test]
    async fn calling_thread_pool_build_with_more_than_zero_threads_returns_ok() {
        let size = 54;
        let pool = ThreadPool::build(size);
        assert!(pool.is_ok_and(|result| result.workers.len() == size));
    }

    #[tokio::test]
    async fn calling_worker_new_returns_new_worker() {
        let id = 23;
        let (sender, receiver) = mpsc::channel::<Job>();
        let (async_sender, async_receiver) = mpsc::channel::<AsyncJob>();
        let receiver = Arc::new(Mutex::new(receiver));
        let async_receiver = Arc::new(tokio::sync::Mutex::new(async_receiver));
        let worker = Worker::new(id, receiver, async_receiver);
        assert_eq!(worker.id, id)
    }

    #[tokio::test]
    async fn calling_thread_pool_execute_should_run_the_closure() {
        let mut stuff: &'static str = "stuff";
        let pool = ThreadPool::new(23);
        let _ = pool.execute(move || {
            stuff = "more stuff";
            assert_eq!(stuff, "more stuff");
        });
    }

    #[tokio::test]
    async fn calling_thread_pool_execute_with_poisoned_mutex_should_not_run_the_closure() {
        let id = 23;
        let (sender, receiver) = mpsc::channel::<Job>();
        let mutex = Arc::new(Mutex::new(receiver));

        let cloned_mutex = Arc::clone(&mutex);
        let _ = thread::spawn(move || {
            let data = cloned_mutex.lock().unwrap();
            panic!();
        }).join();

        let (async_sender, async_receiver) = mpsc::channel::<AsyncJob>();
        let async_mutex = Arc::new(tokio::sync::Mutex::new(async_receiver));

        let worker = Worker::new(id, mutex.clone(), async_mutex.clone());
        let mut pool = ThreadPool::new(1);
        pool.workers[0] = worker;

        let _ = pool.execute(|| {
            panic!("This should not actually panic");
        });
    }

    #[tokio::test]
    async fn calling_thread_pool_execute_async_should_run_closure() {
        let mut stuff: &'static str = "stuff";
        let pool = ThreadPool::new(42);
        let _ = pool.execute_async(Box::pin(async move {
            stuff = "different stuff";
            assert_eq!(stuff, "different stuff")
        }));
    }

    // -----------------------------------------------------------------------
    // The async counterpart of `calling_thread_pool_execute_with_poisoned_mutex_should_not_run_the_closure`
    // (deferred 10/01/2026, now fixed). Two facts shape it:
    //
    // 1. `tokio::sync::Mutex` — unlike `std::sync::Mutex` — has NO poisoning:
    //    a task that panics while holding the guard releases the lock and the
    //    next `lock()` succeeds (see the guard test below). So "poisoned async
    //    mutex" is not a reachable state in this design.
    // 2. What IS reachable, and is the true async analogue, is a job future
    //    that panics inside `get_async_thread`'s loop: the panic unwinds the
    //    whole async worker task, so the loop is gone and no later job runs.
    //    The test below asserts the deferred test's intent — the closure must
    //    not run after the async worker has been killed.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn async_job_panic_terminates_the_async_worker_and_later_jobs_never_run() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let pool = ThreadPool::new(1);

        // Job 1: panics while awaited inside the async worker loop, killing
        // that task (tokio isolates panics to the task; the runtime survives).
        let _ = pool.execute_async(Box::pin(async {
            panic!("poison the async worker loop");
        }));
        // Let the worker pick job 1 up and die with it.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Job 2: must never run — the loop that would have driven it is dead.
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let _ = pool.execute_async(Box::pin(async move {
            flag.store(true, Ordering::SeqCst);
        }));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(
            !ran.load(Ordering::SeqCst),
            "the async worker loop died with the panicking job; \
             a later job must never be driven"
        );
    }

    // Semantic-assumption guard: this test pins WHY the poison formulation
    // above changed. If a future tokio version ever introduces Mutex poisoning,
    // this test fails and the async-worker failure semantics must be
    // re-audited — exactly the situation the deferred test was groping at.
    #[tokio::test]
    async fn tokio_async_mutex_is_not_poisoned_by_a_panicking_holder() {
        let m = Arc::new(tokio::sync::Mutex::new(7u32));
        let m_holder = m.clone();
        let killer = tokio::spawn(async move {
            let _guard = m_holder.lock().await;
            panic!("holder panics while the async mutex is locked");
        });
        // The panicking task is contained: await yields JoinError, not a propagation.
        assert!(killer.await.unwrap_err().is_panic());

        // The lock survived its holder: lock() succeeds and the data is intact —
        // the `std::sync` poisoned-mutex path simply does not exist here.
        let guard = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            m.lock(),
        )
        .await
        .expect("tokio Mutex must not poison: lock() must not block forever");
        assert_eq!(*guard, 7);
        drop(guard);
    }
}