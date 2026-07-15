use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, Thread},
};

use crossbeam_channel::{Receiver, SendError, Sender};
use crossbeam_utils::CachePadded;

use crate::core::DecoderEffect;

#[derive(Debug)]
pub struct AudioBlock {
    pub data: Box<[u8]>,
    pub frames: usize,
    pub pts: i64,
}

pub struct Consumer {
    inner: Arc<AudioQueue>,
    waker: Arc<ThreadWaker>,
    pub stride: usize,
    block_pos: Arc<AtomicUsize>,
}

pub struct Producer(Arc<AudioQueue>);

pub struct WakingSender {
    tx: Sender<DecoderEffect>,
    waker: Arc<ThreadWaker>,
}

pub struct ThreadWaker {
    thread: OnceLock<Thread>,
}

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("buffer full")]
    Full(AudioBlock),
}

struct AudioQueue {
    read: CachePadded<AtomicUsize>,
    write: CachePadded<AtomicUsize>,
    queue: Box<[UnsafeCell<MaybeUninit<AudioBlock>>]>,
    capacity: usize,
}

pub fn queue(capacity: usize, waker: Arc<ThreadWaker>) -> (Producer, Consumer) {
    let inner = Arc::new(AudioQueue::new(capacity));
    (
        Producer(inner.clone()),
        Consumer {
            inner,
            waker,
            stride: 8,
            block_pos: Arc::new(AtomicUsize::new(0)),
        },
    )
}

pub fn channel(waker: Arc<ThreadWaker>) -> (WakingSender, Receiver<DecoderEffect>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (WakingSender::new(tx, waker), rx)
}

impl Consumer {
    pub fn fill(&mut self, rate: usize, buf: &mut [u8]) -> Option<(usize, i64)> {
        let stride = self.stride;
        let mut remaining = buf.len() / stride;
        let mut filled = 0;
        let mut pts = 0;
        let mut frame_pos = 0;

        while remaining > 0 {
            if let Some(block) = self.first() {
                let block_pos = self.block_pos.load(Ordering::Relaxed);
                let unread = block.frames - block_pos;
                let take = unread.min(remaining);

                let buf_pos = filled * stride;
                let read_pos = block_pos * stride;
                let take_bytes = take * stride;

                buf[buf_pos..buf_pos + take_bytes]
                    .copy_from_slice(&block.data[read_pos..read_pos + take_bytes]);

                filled += take;
                remaining -= take;

                let new_block_pos = block_pos + take;
                pts = block.pts;
                frame_pos = new_block_pos;

                if new_block_pos == block.frames {
                    self.pop();
                } else {
                    self.block_pos.store(new_block_pos, Ordering::Relaxed);
                }
            } else {
                break;
            }
        }

        if filled == 0 {
            return None;
        }

        let pts_ns = pts * 1_000_000 + frame_pos as i64 * 1_000_000_000 / rate as i64;
        Some((filled, pts_ns))
    }

    /// # Safety
    ///
    /// Must only be called when no other thread is performing `first()` or `pop()`.
    pub unsafe fn clear(&self) {
        let write = self.inner.write.load(Ordering::Acquire);
        let mut read = self.inner.read.load(Ordering::Relaxed);

        while read != write {
            unsafe {
                (*self.inner.queue[read & (self.inner.capacity - 1)].get()).assume_init_drop();
            }
            read = read.wrapping_add(1);
        }

        self.inner.read.store(write, Ordering::Release);
        self.block_pos.store(0, Ordering::Relaxed);
    }

    fn first(&self) -> Option<&AudioBlock> {
        let read = self.inner.read.load(Ordering::Relaxed);
        let write = self.inner.write.load(Ordering::Acquire);

        if read == write {
            return None;
        }
        unsafe {
            Some((*self.inner.queue[read & (self.inner.capacity - 1)].get()).assume_init_ref())
        }
    }

    fn pop(&mut self) -> Option<AudioBlock> {
        let read = self.inner.read.load(Ordering::Relaxed);
        let write = self.inner.write.load(Ordering::Acquire);

        if read == write {
            return None;
        }
        let block = unsafe {
            (*self.inner.queue[read & (self.inner.capacity - 1)].get()).assume_init_read()
        };
        self.block_pos.store(0, Ordering::Relaxed);
        self.inner
            .read
            .store(read.wrapping_add(1), Ordering::Release);
        self.waker.unpark();
        Some(block)
    }
}

impl Clone for Consumer {
    fn clone(&self) -> Self {
        Consumer {
            inner: self.inner.clone(),
            waker: self.waker.clone(),
            stride: self.stride,
            block_pos: self.block_pos.clone(),
        }
    }
}

impl Producer {
    pub fn try_push(&mut self, block: AudioBlock) -> Result<(), PushError> {
        let write = self.0.write.load(Ordering::Relaxed);
        let read = self.0.read.load(Ordering::Acquire);

        if write.wrapping_sub(read) >= self.0.capacity {
            Err(PushError::Full(block))
        } else {
            unsafe {
                self.0.queue[write & (self.0.capacity - 1)]
                    .get()
                    .write(MaybeUninit::new(block));
            }
            self.0.write.store(write.wrapping_add(1), Ordering::Release);
            Ok(())
        }
    }
}

impl WakingSender {
    pub fn new(tx: Sender<DecoderEffect>, waker: Arc<ThreadWaker>) -> Self {
        Self { tx, waker }
    }

    pub fn send(&self, msg: DecoderEffect) -> Result<(), SendError<DecoderEffect>> {
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

impl AudioQueue {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.next_power_of_two();
        let mut queue = Vec::with_capacity(capacity);
        unsafe { queue.set_len(capacity) };
        Self {
            queue: queue.into_boxed_slice(),
            read: CachePadded::new(AtomicUsize::new(0)),
            write: CachePadded::new(AtomicUsize::new(0)),
            capacity,
        }
    }
}

unsafe impl Sync for AudioQueue {}

impl Drop for AudioQueue {
    fn drop(&mut self) {
        let write = self.write.load(Ordering::Relaxed);
        let mut read = self.read.load(Ordering::Relaxed);

        while read != write {
            unsafe {
                (*self.queue[read & (self.capacity - 1)].get()).assume_init_drop();
            }
            read = read.wrapping_add(1);
        }
    }
}
