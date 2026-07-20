use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    thread::{self, Thread},
};

use crossbeam_channel::{Receiver, SendError, Sender};

use crate::engine::get_engine_start;

#[derive(Debug)]
pub struct Clock {
    pub active: AtomicBool,
    written_pts: AtomicI64,
    pub end_time: AtomicI64,
}

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

impl Clock {
    pub fn update(&self, pts: i64, end_time: i64) {
        self.written_pts.store(pts, Ordering::Relaxed);
        self.end_time.store(end_time, Ordering::Relaxed);
    }
    pub fn get_ms(&self) -> i64 {
        let pts = self.written_pts.load(Ordering::Relaxed);
        let end_time = self.end_time.load(Ordering::Relaxed);
        let now_time = get_engine_start().elapsed().as_nanos() as i64;
        (pts - 0.max(end_time - now_time)) / 1_000_000
    }
}

impl Default for Clock {
    fn default() -> Self {
        let time = get_engine_start().elapsed().as_nanos() as i64;
        Self {
            active: AtomicBool::new(false),
            written_pts: AtomicI64::default(),
            end_time: AtomicI64::new(time),
        }
    }
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
