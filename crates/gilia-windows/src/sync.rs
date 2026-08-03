// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    mem::ManuallyDrop,
    sync::Arc,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvError, RecvTimeoutError, SendError, Sender, TryRecvError};
use snafu::{OptionExt, ResultExt, Snafu, ensure};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_TIMEOUT},
    System::{
        SystemServices::MAXIMUM_WAIT_OBJECTS,
        Threading::{
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateEventW,
            CreateWaitableTimerExW, INFINITE, SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS,
            WaitForMultipleObjects,
        },
    },
};

#[derive(Clone, Debug, Snafu)]
pub enum SyncError {
    #[snafu(display("cannot wait on {count} handles"))]
    InvalidWaitCount { count: usize },
    #[snafu(display("failed to wait on handles"))]
    WaitFailed { source: windows::core::Error },
    #[snafu(display("invalid wait result: {result}"))]
    InvalidWaitResult { result: u32 },
    #[snafu(display("wait timed out"))]
    WaitTimeout,
    #[snafu(display("failed to create event handle"))]
    CreateEvent { source: windows::core::Error },
    #[snafu(display("failed to signal event handle"))]
    SignalEvent { source: windows::core::Error },
    #[snafu(display("failed to create waitable timer"))]
    CreateTimer { source: windows::core::Error },
    #[snafu(display("failed to set waitable timer"))]
    SetTimer { source: windows::core::Error },
}

#[derive(Debug, Snafu)]
pub enum EventSendError<T> {
    #[snafu(display("failed to send message to channel"))]
    Channel { msg: T },
    #[snafu(display("message queued but could not signal"))]
    Signal { source: SyncError },
}

#[derive(Debug, Snafu)]
pub enum EventRecvTimeoutError {
    #[snafu(transparent)]
    Sync { source: SyncError },
    #[snafu(display("timed out waiting on channel"))]
    Timeout,
    #[snafu(display("channel disconnected"))]
    Disconnected,
}

pub struct EventSender<T> {
    tx: ManuallyDrop<Sender<T>>,
    event: WakeEvent,
}

pub struct EventReceiver<T> {
    rx: Receiver<T>,
    event: WakeEvent,
    timer: Option<WaitableTimer>,
}

#[derive(Clone)]
pub struct WakeEvent(Arc<EventHandle>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitResult {
    Object(usize),
    Timeout,
}

struct WaitableTimer(HANDLE);

struct EventHandle(HANDLE);

pub fn event_channel<T>() -> Result<(EventSender<T>, EventReceiver<T>), SyncError> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let event = WakeEvent::new()?;
    Ok((
        EventSender {
            tx: ManuallyDrop::new(tx),
            event: event.clone(),
        },
        EventReceiver {
            rx,
            event,
            timer: None,
        },
    ))
}

pub fn timed_channel<T>() -> Result<(EventSender<T>, EventReceiver<T>), SyncError> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let event = WakeEvent::new()?;
    let timer = WaitableTimer::new()?;
    Ok((
        EventSender {
            tx: ManuallyDrop::new(tx),
            event: event.clone(),
        },
        EventReceiver {
            rx,
            event,
            timer: Some(timer),
        },
    ))
}

pub(crate) fn wait_for_multiple(handles: &[HANDLE], timeout: u32) -> Result<WaitResult, SyncError> {
    ensure!(
        (1..=MAXIMUM_WAIT_OBJECTS as usize).contains(&handles.len()),
        InvalidWaitCountSnafu {
            count: handles.len()
        }
    );

    let res = unsafe { WaitForMultipleObjects(handles, false, timeout) };

    if res == WAIT_FAILED {
        return Err(SyncError::WaitFailed {
            source: windows::core::Error::from_thread(),
        });
    }

    if res == WAIT_TIMEOUT {
        return Ok(WaitResult::Timeout);
    }

    let index = res.0 as usize;

    ensure!(
        index < handles.len(),
        InvalidWaitResultSnafu { result: res.0 }
    );

    Ok(WaitResult::Object(index))
}

impl<T> From<SendError<T>> for EventSendError<T> {
    fn from(SendError(msg): SendError<T>) -> Self {
        Self::Channel { msg }
    }
}

impl<T> From<SyncError> for EventSendError<T> {
    fn from(source: SyncError) -> Self {
        Self::Signal { source }
    }
}

impl<T> EventSender<T> {
    pub fn send(&self, msg: T) -> Result<(), EventSendError<T>> {
        self.tx.send(msg)?;
        self.event.signal()?;
        Ok(())
    }
}

impl<T> Drop for EventSender<T> {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.tx);
        }
        let _ = self.event.signal();
    }
}

impl<T> EventReceiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.rx.try_recv()
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        self.rx.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, EventRecvTimeoutError> {
        match Instant::now().checked_add(timeout) {
            Some(deadline) => self.recv_deadline(deadline),
            None => self.recv().ok().context(DisconnectedSnafu),
        }
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, EventRecvTimeoutError> {
        let Some(timer) = &self.timer else {
            return match self.rx.recv_deadline(deadline) {
                Ok(val) => Ok(val),
                Err(RecvTimeoutError::Timeout) => TimeoutSnafu.fail(),
                Err(RecvTimeoutError::Disconnected) => DisconnectedSnafu.fail(),
            };
        };

        let now = Instant::now();
        if deadline <= now {
            return match self.rx.try_recv() {
                Ok(val) => Ok(val),
                Err(TryRecvError::Empty) => TimeoutSnafu.fail(),
                Err(TryRecvError::Disconnected) => DisconnectedSnafu.fail(),
            };
        }

        let duration = deadline.saturating_duration_since(now);
        timer.set(duration)?;

        loop {
            match self.rx.try_recv() {
                Ok(val) => {
                    timer.cancel();
                    return Ok(val);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    timer.cancel();
                    return DisconnectedSnafu.fail();
                }
            }

            let handles = [self.event.raw(), timer.raw()];
            match wait_for_multiple(&handles, INFINITE) {
                Ok(WaitResult::Object(0)) => {}
                Ok(WaitResult::Object(1)) => {
                    return match self.rx.try_recv() {
                        Ok(val) => Ok(val),
                        Err(TryRecvError::Empty) => TimeoutSnafu.fail(),
                        Err(TryRecvError::Disconnected) => DisconnectedSnafu.fail(),
                    };
                }
                Ok(WaitResult::Object(_)) => unreachable!(),
                Ok(WaitResult::Timeout) => {
                    timer.cancel();
                    Err(SyncError::WaitTimeout)?;
                }
                Err(source) => {
                    timer.cancel();
                    Err(source)?;
                }
            }
        }
    }

    pub fn event(&self) -> &WakeEvent {
        &self.event
    }

    pub fn rx(&self) -> &Receiver<T> {
        &self.rx
    }
}

impl WakeEvent {
    pub fn new() -> Result<Self, SyncError> {
        let handle = unsafe { CreateEventW(None, false, false, None).context(CreateEventSnafu)? };
        Ok(Self(Arc::new(EventHandle(handle))))
    }

    pub fn signal(&self) -> Result<(), SyncError> {
        unsafe { SetEvent(self.0.0).context(SignalEventSnafu) }
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.0.0
    }
}

impl WaitableTimer {
    fn new() -> Result<Self, SyncError> {
        let access = TIMER_ALL_ACCESS.0;
        let handle = match unsafe {
            CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, access)
        } {
            Ok(handle) => handle,
            Err(_) => unsafe {
                CreateWaitableTimerExW(None, None, 0, access).context(CreateTimerSnafu)?
            },
        };
        Ok(Self(handle))
    }

    fn set(&self, duration: Duration) -> Result<(), SyncError> {
        let ticks = duration.as_nanos().div_ceil(100).clamp(1, i64::MAX as u128) as i64;
        let due_time = -ticks;
        unsafe {
            SetWaitableTimer(self.0, &raw const due_time, 0, None, None, false)
                .context(SetTimerSnafu)
        }
    }

    fn cancel(&self) {
        let _ = unsafe { CancelWaitableTimer(self.0) };
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

unsafe impl Send for WaitableTimer {}

impl Drop for WaitableTimer {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

impl Drop for EventHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
