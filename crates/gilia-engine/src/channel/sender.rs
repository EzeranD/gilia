// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    fmt, process,
    sync::{Arc, atomic::Ordering},
};

use crossbeam_utils::Backoff;
use snafu::{Snafu, ensure};

use super::{
    Channel,
    wait::{ThreadWaiter, WaitError, WaitSource, Waiter, WaiterKind, WakeMode, wait_until},
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum SendError<T> {
    #[snafu(display("channel is disconnected"))]
    Disconnected { value: T },
    #[snafu(display("failed to wait for channel capacity"))]
    Wait { source: WaitError, value: T },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum TrySendError<T> {
    #[snafu(display("channel is full"))]
    Full { value: T },
    #[snafu(display("channel is disconnected"))]
    Disconnected { value: T },
}

pub struct Sender<T> {
    pub(super) inner: Arc<Channel<T>>,
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let mut item = value;
        let backoff = Backoff::new();
        let mut waiter = None;

        loop {
            match self.try_send(item) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full { value }) => item = value,
                Err(TrySendError::Disconnected { value }) => {
                    return Err(SendError::Disconnected { value });
                }
            }

            if backoff.is_completed() {
                let waiter = waiter.get_or_insert_with(ThreadWaiter::acquire);
                if let Err(source) =
                    wait_until(&[self], waiter.as_waiter(), WaiterKind::Operation, None)
                {
                    return Err(SendError::Wait {
                        source,
                        value: item,
                    });
                }

                backoff.reset();
            } else {
                backoff.snooze();
            }
        }
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        ensure!(
            !self.is_disconnected(),
            try_send_error::DisconnectedSnafu { value }
        );

        self.inner
            .queue
            .push(value)
            .map_err(|value| TrySendError::Full { value })?;

        if self.is_disconnected() {
            self.inner.clear();
        }

        self.inner.wake_receivers(WakeMode::OneOperation);

        Ok(())
    }

    pub fn is_disconnected(&self) -> bool {
        self.inner.receivers.load(Ordering::Acquire) == 0
    }
}

impl<T> WaitSource for Sender<T> {
    fn is_ready(&self) -> bool {
        self.is_disconnected() || !self.inner.queue.is_full()
    }

    fn register(&self, waiter: &Arc<dyn Waiter>, kind: WaiterKind) {
        self.inner.send_waiters.register(waiter, kind);
    }

    fn unregister(&self, waiter: &Arc<dyn Waiter>) {
        self.inner.send_waiters.unregister(waiter);
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        if self.inner.senders.fetch_add(1, Ordering::Relaxed) >= isize::MAX as usize {
            process::abort();
        }

        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.inner.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.wake_receivers(WakeMode::All);
        }
    }
}
