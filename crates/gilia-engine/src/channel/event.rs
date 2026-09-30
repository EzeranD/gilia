// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    cell::Cell,
    marker::PhantomData,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Instant,
};

use gilia_windows::{
    ControlReceiver, HANDLE, SyncError, WaitableTimer, WakeEvent, WasapiError,
    sync::{WaitResult, wait_for_multiple},
};
use snafu::ResultExt;

use super::{
    Receiver, TryRecvError,
    wait::{WaitError, WaitOutcome, WaitSource, Waiter, WaiterKind, WakeError},
};

pub struct EventReceiver<T> {
    inner: Receiver<T>,
    event: Arc<EventWaiter>,
}

#[derive(Debug)]
pub(super) struct EventWaiter {
    event: WakeEvent,
    timer: Mutex<Option<WaitableTimer>>,
}

pub(super) struct EventWaiterGuard {
    pair: Option<EventWaiterPair>,
    _not_send: PhantomData<Rc<()>>,
}

type EventWaiterPair = (Arc<EventWaiter>, Arc<dyn Waiter>);

thread_local! {
    static CACHED_EVENT_WAITER: Cell<Option<EventWaiterPair>> = const {
        Cell::new(None)
    };
}

impl<T> EventReceiver<T> {
    pub(super) fn new(inner: Receiver<T>) -> Result<Self, SyncError> {
        Ok(EventReceiver {
            inner,
            event: Arc::new(EventWaiter::new()?),
        })
    }

    fn as_waiter(&self) -> Arc<dyn Waiter> {
        self.event.clone()
    }
}

impl<T> ControlReceiver for EventReceiver<T> {
    type Message = T;

    fn try_recv(&self) -> Result<Option<T>, WasapiError> {
        match self.inner.try_recv() {
            Ok(message) => return Ok(Some(message)),
            Err(TryRecvError::Disconnected) => return Err(WasapiError::Disconnected),
            Err(TryRecvError::Empty) => {}
        }

        let waiter = self.as_waiter();
        self.inner.register(&waiter, WaiterKind::Observer);

        match self.inner.try_recv() {
            Ok(value) => Ok(Some(value)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(WasapiError::Disconnected),
        }
    }

    fn raw(&self) -> HANDLE {
        self.event.raw()
    }
}

impl<T> Drop for EventReceiver<T> {
    fn drop(&mut self) {
        self.inner.unregister(&self.as_waiter());
    }
}

impl EventWaiter {
    pub(super) fn new() -> Result<Self, SyncError> {
        Ok(Self {
            event: WakeEvent::new()?,
            timer: Mutex::new(None),
        })
    }

    pub(super) fn raw(&self) -> gilia_windows::HANDLE {
        self.event.raw()
    }

    fn new_pair() -> Result<EventWaiterPair, SyncError> {
        let concrete = Arc::new(Self::new()?);
        let waiter: Arc<dyn Waiter> = concrete.clone();

        Ok((concrete, waiter))
    }

    pub(super) fn acquire() -> Result<EventWaiterGuard, SyncError> {
        let cached = CACHED_EVENT_WAITER.try_with(Cell::take).ok().flatten();
        let pair = match cached {
            Some(pair) => pair,
            None => Self::new_pair()?,
        };

        Ok(EventWaiterGuard {
            pair: Some(pair),
            _not_send: PhantomData,
        })
    }
}

impl Waiter for EventWaiter {
    fn wake(&self) -> Result<(), WakeError> {
        self.event.signal().boxed().map_err(Into::into)
    }

    fn wait(&self, deadline: Option<Instant>) -> Result<WaitOutcome, WaitError> {
        let Some(deadline) = deadline else {
            let outcome = match self.event.wait(None).boxed()? {
                WaitResult::Object(0) => WaitOutcome::Retry,
                WaitResult::Timeout => WaitOutcome::Deadline,
                WaitResult::Object(_) => unreachable!(),
            };

            return Ok(outcome);
        };

        if Instant::now() >= deadline {
            return Ok(WaitOutcome::Deadline);
        }

        let mut guard = self.timer.lock().unwrap();

        let timer = match guard.as_mut() {
            Some(timer) => timer,
            None => guard.insert(WaitableTimer::new().boxed()?),
        };

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(WaitOutcome::Deadline);
        }

        timer.set(remaining).boxed()?;

        let result = wait_for_multiple(&[self.event.raw(), timer.raw()], u32::MAX);

        timer.cancel();

        let outcome = match result.boxed()? {
            WaitResult::Object(0) => WaitOutcome::Retry,
            _ => WaitOutcome::Deadline,
        };

        Ok(outcome)
    }
}

impl EventWaiterGuard {
    pub(super) fn as_waiter(&self) -> &Arc<dyn Waiter> {
        &self.pair.as_ref().unwrap().1
    }
}

impl Drop for EventWaiterGuard {
    fn drop(&mut self) {
        let Some(pair) = self.pair.take() else {
            return;
        };

        let _ = CACHED_EVENT_WAITER.try_with(|cache| {
            let cached = cache.take();
            cache.set(cached.or(Some(pair)));
        });
    }
}
