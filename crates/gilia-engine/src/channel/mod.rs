// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, atomic::AtomicUsize};

use crossbeam_queue::{ArrayQueue, SegQueue};
use tracing::warn;

use self::wait::{WaiterList, WakeMode};

#[cfg(windows)]
mod event;
#[cfg(target_os = "linux")]
mod pipe;
mod receiver;
pub(crate) mod select;
mod sender;
pub(crate) mod wait;

#[cfg(windows)]
pub use event::EventReceiver;
#[cfg(target_os = "linux")]
pub use pipe::{PipeError, PipeReceiver, PipeWireError};
pub use receiver::{Receiver, RecvError, RecvTimeoutError, TryRecvError};
pub(crate) use select::select_biased;
pub use sender::{SendError, Sender, TrySendError};

pub(crate) use self::wait::WaitSource;

pub struct Builder {
    capacity: Capacity,
}

struct Channel<T> {
    queue: ChannelQueue<T>,
    senders: AtomicUsize,
    receivers: AtomicUsize,
    send_waiters: WaiterList,
    recv_waiters: WaiterList,
}

enum Capacity {
    Bounded(usize),
    Unbounded,
}

enum ChannelQueue<T> {
    Bounded(ArrayQueue<T>),
    Unbounded(SegQueue<T>),
}

impl Builder {
    pub fn bounded(capacity: usize) -> Self {
        assert!(capacity > 0, "channel capacity must be greater than 0");

        Self {
            capacity: Capacity::Bounded(capacity),
        }
    }

    pub fn unbounded() -> Self {
        Self {
            capacity: Capacity::Unbounded,
        }
    }

    pub fn channel<T>(self) -> (Sender<T>, Receiver<T>) {
        let queue = match self.capacity {
            Capacity::Bounded(capacity) => ChannelQueue::Bounded(ArrayQueue::new(capacity)),
            Capacity::Unbounded => ChannelQueue::Unbounded(SegQueue::new()),
        };

        let inner = Arc::new(Channel {
            queue,
            senders: AtomicUsize::new(1),
            receivers: AtomicUsize::new(1),
            send_waiters: WaiterList::new(),
            recv_waiters: WaiterList::new(),
        });

        (
            Sender {
                inner: inner.clone(),
            },
            Receiver { inner },
        )
    }

    #[cfg(target_os = "linux")]
    pub fn pipewire<T>(self) -> Result<(Sender<T>, PipeReceiver<T>), PipeError> {
        let (sender, receiver) = self.channel();

        Ok((sender, PipeReceiver::new(receiver)?))
    }

    #[cfg(windows)]
    pub fn wasapi<T>(self) -> Result<(Sender<T>, EventReceiver<T>), gilia_windows::SyncError> {
        let (sender, receiver) = self.channel();

        Ok((sender, EventReceiver::new(receiver)?))
    }
}

impl<T> Channel<T> {
    fn wake_receivers(&self, mode: WakeMode) {
        if let Err(e) = self.recv_waiters.wake(mode) {
            warn!("Failed to wake channel receivers: {e}");
        }
    }

    fn wake_senders(&self, mode: WakeMode) {
        if let Err(e) = self.send_waiters.wake(mode) {
            warn!("Failed to wake channel senders: {e}");
        }
    }

    fn clear(&self) {
        while self.queue.pop().is_some() {}
    }
}

impl<T> ChannelQueue<T> {
    fn push(&self, value: T) -> Result<(), T> {
        match self {
            Self::Bounded(queue) => queue.push(value),
            Self::Unbounded(queue) => {
                queue.push(value);
                Ok(())
            }
        }
    }

    fn pop(&self) -> Option<T> {
        match self {
            Self::Bounded(queue) => queue.pop(),
            Self::Unbounded(queue) => queue.pop(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Bounded(queue) => queue.is_empty(),
            Self::Unbounded(queue) => queue.is_empty(),
        }
    }

    fn is_full(&self) -> bool {
        match self {
            Self::Bounded(queue) => queue.is_full(),
            Self::Unbounded(_) => false,
        }
    }
}
