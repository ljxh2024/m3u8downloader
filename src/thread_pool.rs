/// 线程池实现

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
};

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 线程池
pub struct ThreadPool {
  pub workers: Vec<Worker>,
  sender: Option<mpsc::Sender<Job>>,
}

/// 线程池管理
impl ThreadPool {
    pub fn new(size: usize) -> Self {
        let (sender, receiver) = mpsc::channel();
        let receiver = Arc::new(Mutex::new(receiver));

        let mut workers = Vec::with_capacity(size);
        for id in 0..size {
            workers.push(Worker::new(id, Arc::clone(&receiver)));
        }
        ThreadPool { workers, sender: Some(sender) }
    }

    pub fn execute<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let job = Box::new(f);
        self.sender.as_ref().unwrap().send(job).unwrap();
    }
}

/// 优雅停机与清理
impl Drop for ThreadPool {
    fn drop(&mut self) {
        drop(self.sender.take());

        for worker in self.workers.drain(..) {
            println!("Shutting down worker {}", worker.id);
            worker.thread.join().unwrap();
        }
    }
}

/// 工作的线程
pub struct Worker {
    pub id: usize,
    pub thread: thread::JoinHandle<()>,
}

impl Worker {
    fn new(id: usize, receiver: Arc<Mutex<mpsc::Receiver<Job>>>) -> Self {
        let thread = thread::spawn(move || {
            loop {
                let message = receiver.lock().unwrap().recv();
                
                match message {
                    Ok(job) => {
                        // println!("Worker {} got a job; executing.", id);
                        job();
                    }
                    Err(_) => {
                        // println!("Worker {} disconnected; shutting down.", id);
                        break;
                    }
                }
            }
        });
        Worker { id, thread }
    }
}