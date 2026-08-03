// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(target_os = "windows")]

pub mod audio;
pub mod com;
pub mod sync;

pub use audio::{OutputBuffer, OutputTiming, WasapiError, WasapiEvent, WasapiStream};
pub use com::{Com, MmcssRegistration};
pub use sync::{
    EventReceiver, EventRecvTimeoutError, EventSendError, EventSender, SyncError, WakeEvent,
    event_channel, timed_channel,
};
pub use windows_core::w;
