// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    fmt,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use crossbeam_utils::Backoff;
use snafu::{OptionExt, ResultExt, Snafu, ensure};

use super::{
    Channel, ChannelQueue,
    wait::{
        ThreadWaiter, WaitError, WaitSource, Waiter, WaiterKind, WakeMode, wait_ready, wait_until,
    },
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RecvError {
    #[snafu(display("channel is disconnected"))]
    Disconnected,
    #[snafu(display("failed to wait for a message"))]
    Wait { source: WaitError },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Snafu)]
#[snafu(module)]
pub enum TryRecvError {
    #[snafu(display("receiving on an empty channel"))]
    Empty,
    #[snafu(display("channel is disconnected"))]
    Disconnected,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RecvTimeoutError {
    #[snafu(display("receive timed out"))]
    Timeout,
    #[snafu(display("channel is disconnected"))]
    Disconnected,
    #[snafu(display("failed to wait for a message"))]
    Wait { source: WaitError },
}

pub struct Receiver<T> {
    pub(super) inner: Arc<Channel<T>>,
}

impl<T> Receiver<T> {
    pub fn recv(&self) -> Result<T, RecvError> {
        let backoff = Backoff::new();
        let mut waiter = None;

        loop {
            match self.try_recv() {
                Ok(value) => return Ok(value),
                Err(TryRecvError::Disconnected) => return Err(RecvError::Disconnected),
                Err(TryRecvError::Empty) => {}
            }

            if backoff.is_completed() {
                let waiter = waiter.get_or_insert_with(ThreadWaiter::acquire);
                wait_until(&[self], waiter.as_waiter(), WaiterKind::Operation, None)
                    .context(recv_error::WaitSnafu)?;

                backoff.reset();
            } else {
                backoff.snooze();
            }
        }
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let value = match self.inner.queue.pop() {
            Some(value) => value,
            None if self.is_disconnected() => self
                .inner
                .queue
                .pop()
                .context(try_recv_error::DisconnectedSnafu)?,
            None => return try_recv_error::EmptySnafu.fail(),
        };

        if matches!(&self.inner.queue, ChannelQueue::Bounded(_)) {
            self.inner.wake_senders(WakeMode::OneOperation);
        }

        Ok(value)
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        match Instant::now().checked_add(timeout) {
            Some(deadline) => self.recv_deadline(deadline),
            None => self.recv().map_err(|e| match e {
                RecvError::Disconnected => RecvTimeoutError::Disconnected,
                RecvError::Wait { source } => RecvTimeoutError::Wait { source },
            }),
        }
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        let backoff = Backoff::new();

        loop {
            match self.try_recv() {
                Ok(value) => return Ok(value),
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
                Err(TryRecvError::Empty) => {}
            }

            ensure!(Instant::now() < deadline, recv_timeout_error::TimeoutSnafu);

            if backoff.is_completed() {
                wait_ready(self, deadline).context(recv_timeout_error::WaitSnafu)?;
                backoff.reset();
            } else {
                backoff.snooze();
            }
        }
    }

    pub fn is_disconnected(&self) -> bool {
        self.inner.senders.load(Ordering::Acquire) == 0
    }
}

impl<T> WaitSource for Receiver<T> {
    fn is_ready(&self) -> bool {
        self.is_disconnected() || !self.inner.queue.is_empty()
    }

    fn register(&self, waiter: &Arc<dyn Waiter>, kind: WaiterKind) {
        self.inner.recv_waiters.register(waiter, kind);
    }

    fn unregister(&self, waiter: &Arc<dyn Waiter>) {
        self.inner.recv_waiters.unregister(waiter);
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver").finish_non_exhaustive()
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        if self.inner.receivers.fetch_add(1, Ordering::Relaxed) >= isize::MAX as usize {
            std::process::abort();
        }

        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.inner.receivers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.wake_senders(WakeMode::All);
            self.inner.clear();
        }
    }
}
