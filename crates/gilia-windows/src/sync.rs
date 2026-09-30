// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{sync::Arc, time::Duration};

use snafu::{ResultExt, Snafu, ensure};
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

#[derive(Debug, Clone, Snafu)]
pub enum SyncError {
    #[snafu(display("cannot wait on {count} handles"))]
    InvalidWaitCount { count: usize },
    #[snafu(display("failed to wait on handles"))]
    WaitFailed { source: windows::core::Error },
    #[snafu(display("invalid wait result: {result}"))]
    InvalidWaitResult { result: u32 },
    #[snafu(display("failed to create event handle"))]
    CreateEvent { source: windows::core::Error },
    #[snafu(display("failed to signal event handle"))]
    SignalEvent { source: windows::core::Error },
    #[snafu(display("failed to create waitable timer"))]
    CreateTimer { source: windows::core::Error },
    #[snafu(display("failed to set waitable timer"))]
    SetTimer { source: windows::core::Error },
}

#[derive(Debug, Clone)]
pub struct WakeEvent(Arc<EventHandle>);

#[derive(Debug)]
pub struct WaitableTimer(HANDLE);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitResult {
    Object(usize),
    Timeout,
}

#[derive(Debug)]
struct EventHandle(HANDLE);

pub fn wait_for_multiple(handles: &[HANDLE], timeout: u32) -> Result<WaitResult, SyncError> {
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

impl WakeEvent {
    pub fn new() -> Result<Self, SyncError> {
        let handle = unsafe { CreateEventW(None, false, false, None).context(CreateEventSnafu)? };
        Ok(Self(Arc::new(EventHandle(handle))))
    }

    pub fn signal(&self) -> Result<(), SyncError> {
        unsafe { SetEvent(self.0.0).context(SignalEventSnafu) }
    }

    pub fn wait(&self, timeout: Option<Duration>) -> Result<WaitResult, SyncError> {
        let timeout = timeout.map_or(INFINITE, |timeout| {
            timeout
                .as_nanos()
                .div_ceil(1_000_000)
                .min((INFINITE - 1) as u128) as u32
        });

        wait_for_multiple(&[self.raw()], timeout)
    }

    pub fn raw(&self) -> HANDLE {
        self.0.0
    }
}

impl WaitableTimer {
    pub fn new() -> Result<Self, SyncError> {
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

    pub fn set(&self, duration: Duration) -> Result<(), SyncError> {
        let ticks = duration.as_nanos().div_ceil(100).clamp(1, i64::MAX as u128) as i64;
        let due_time = -ticks;
        unsafe {
            SetWaitableTimer(self.0, &raw const due_time, 0, None, None, false)
                .context(SetTimerSnafu)
        }
    }

    pub fn cancel(&self) {
        let _ = unsafe { CancelWaitableTimer(self.0) };
    }

    pub fn raw(&self) -> HANDLE {
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
