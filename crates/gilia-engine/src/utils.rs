// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    thread::{self, Thread},
};

use crossbeam_channel::{Receiver, SendError, Sender};
use ffmpeg_next::Packet;

use crate::engine::get_engine_start;

#[derive(Debug)]
pub struct Clock {
    pub active: AtomicBool,
    written_pts: AtomicI64,
    pub end_time: AtomicI64,
}

#[derive(Debug, Clone)]
pub struct MemoryBudget {
    current_bytes: Arc<AtomicUsize>,
    max_bytes: usize,
    waker: Arc<ThreadWaker>,
}

pub struct TrackedPacket {
    pub packet: Packet,
    pub budget: MemoryBudget,
}

pub struct UnparkSender<T> {
    tx: Sender<T>,
    waker: Arc<ThreadWaker>,
}

#[derive(Debug)]
pub struct ThreadWaker {
    thread: OnceLock<Thread>,
}

pub fn unpark_channel<T>(waker: Arc<ThreadWaker>) -> (UnparkSender<T>, Receiver<T>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (UnparkSender { tx, waker }, rx)
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

impl MemoryBudget {
    pub fn new(max_bytes: usize, waker: Arc<ThreadWaker>) -> Self {
        let current_bytes = Arc::new(AtomicUsize::new(0));
        Self {
            current_bytes,
            max_bytes,
            waker,
        }
    }

    pub fn add(&self, bytes: usize) {
        self.current_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn sub(&self, bytes: usize) {
        let prev = self.current_bytes.fetch_sub(bytes, Ordering::Relaxed);
        let current = prev.saturating_sub(bytes);
        if prev >= self.max_bytes && current < self.max_bytes {
            self.waker.unpark();
        }
    }

    pub fn over_limit(&self) -> bool {
        self.current_bytes.load(Ordering::Relaxed) >= self.max_bytes
    }

    pub fn current_bytes(&self) -> usize {
        self.current_bytes.load(Ordering::Relaxed)
    }
}

impl TrackedPacket {
    pub fn new(packet: Packet, budget: MemoryBudget) -> Self {
        budget.add(packet.size());
        Self { packet, budget }
    }
}

impl DerefMut for TrackedPacket {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.packet
    }
}

impl Deref for TrackedPacket {
    type Target = Packet;

    fn deref(&self) -> &Self::Target {
        &self.packet
    }
}

impl Drop for TrackedPacket {
    fn drop(&mut self) {
        self.budget.sub(self.packet.size());
    }
}

impl<T> UnparkSender<T> {
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
