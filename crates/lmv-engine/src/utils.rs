use std::{
    sync::{Arc, OnceLock},
    thread::{self, Thread},
};

use crossbeam_channel::{Receiver, SendError, Sender};

pub struct WakingSender<T> {
    tx: Sender<T>,
    waker: Arc<ThreadWaker>,
}

pub struct ThreadWaker {
    thread: OnceLock<Thread>,
}

pub fn channel<T>(waker: Arc<ThreadWaker>) -> (WakingSender<T>, Receiver<T>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (WakingSender::new(tx, waker), rx)
}

impl<T> WakingSender<T> {
    pub fn new(tx: Sender<T>, waker: Arc<ThreadWaker>) -> Self {
        Self { tx, waker }
    }

    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        self.tx.send(msg)?;
        self.waker.unpark();
        Ok(())
    }
}

impl ThreadWaker {
    pub fn new() -> Self {
        Self {
            thread: OnceLock::new(),
        }
    }
    pub fn set(&self) {
        self.thread
            .set(thread::current())
            .expect("Thread set twice");
    }
    pub fn unpark(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
}
