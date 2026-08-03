// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

use snafu::{ResultExt, Snafu};
use tracing::warn;
use windows::Win32::{Foundation::HANDLE, System::Threading::INFINITE};

use super::{
    client::{AudioClient, ClientError, OutputBuffer},
    device::DeviceManager,
};
use crate::sync::{EventReceiver, SyncError, WaitResult, WakeEvent, wait_for_multiple};

#[derive(Debug, Snafu)]
pub enum WasapiError {
    #[snafu(display("failed to create WASAPI stream events"))]
    CreateEvents { source: SyncError },
    #[snafu(display("control channel disconnected"))]
    Disconnected,
    #[snafu(display("failed while waiting for WASAPI events"))]
    WaitEvents { source: SyncError },
    #[snafu(display("failed to check whether the WASAPI stream is empty"))]
    Empty { source: ClientError },
    #[snafu(display("failed to acquire WASAPI output buffer"))]
    AcquireBuffer { source: ClientError },
}

pub struct WasapiStream {
    device_manager: Option<DeviceManager>,
    connection: Option<AudioClient>,
    events: StreamEvents,
    rate: u32,
    volume: Option<f32>,
    retry_delay: Duration,
    active: bool,
}

struct StreamEvents {
    device: WakeEvent,
    audio: WakeEvent,
    volume: WakeEvent,
    disconnect: WakeEvent,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WasapiEvent<T> {
    Audio,
    Control(T),
    Volume(f32),
    Connected(bool),
}

impl WasapiStream {
    pub fn new(rate: u32, volume: Option<f32>) -> Result<Self, WasapiError> {
        Ok(Self {
            device_manager: None,
            connection: None,
            events: StreamEvents::new().context(CreateEventsSnafu)?,
            rate,
            volume,
            retry_delay: Duration::from_millis(100),
            active: true,
        })
    }

    pub fn connect(&mut self) -> bool {
        self.disconnect();

        if self.device_manager.is_none() {
            let manager = match DeviceManager::new(self.events.device.clone()) {
                Ok(manager) => manager,
                Err(e) => {
                    warn!(
                        "device manager unavailable (retrying in {:?}): {e}",
                        self.retry_delay
                    );
                    self.schedule_retry();
                    return false;
                }
            };
            self.device_manager = Some(manager);
        }

        let Some(manager) = &self.device_manager else {
            return false;
        };

        let device = match manager.default_device() {
            Ok(device) => device,
            Err(e) => {
                warn!(
                    "default audio device unavailable (retrying in {:?}): {e}",
                    self.retry_delay
                );
                self.schedule_retry();
                return false;
            }
        };

        let (connection, volume) = match AudioClient::new(
            &device,
            self.rate,
            self.volume,
            self.active,
            &self.events.audio,
            &self.events.volume,
            &self.events.disconnect,
        ) {
            Ok(result) => result,
            Err(e) => {
                warn!(
                    "audio client initialization failed (retrying in {:?}): {e}",
                    self.retry_delay
                );
                self.schedule_retry();
                return false;
            }
        };

        self.volume = Some(volume);
        self.connection = Some(connection);

        self.retry_delay = Duration::from_millis(100);
        true
    }

    pub fn wait<T>(&mut self, control: &EventReceiver<T>) -> Result<WasapiEvent<T>, WasapiError> {
        loop {
            match control.try_recv() {
                Ok(msg) => return Ok(WasapiEvent::Control(msg)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    return Err(WasapiError::Disconnected);
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }

            let (handles, timeout, count) = match &self.connection {
                Some(_) => (
                    [
                        control.event().raw(),
                        self.events.device.raw(),
                        self.events.audio.raw(),
                        self.events.volume.raw(),
                        self.events.disconnect.raw(),
                    ],
                    INFINITE,
                    5,
                ),
                None => (
                    [
                        control.event().raw(),
                        self.events.device.raw(),
                        HANDLE::default(),
                        HANDLE::default(),
                        HANDLE::default(),
                    ],
                    self.retry_delay.as_millis() as u32,
                    2,
                ),
            };

            match wait_for_multiple(&handles[..count], timeout).context(WaitEventsSnafu)? {
                WaitResult::Object(0) => match control.try_recv() {
                    Ok(msg) => return Ok(WasapiEvent::Control(msg)),
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        return Err(WasapiError::Disconnected);
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => {}
                },
                WaitResult::Object(2) => return Ok(WasapiEvent::Audio),
                WaitResult::Object(3) => return Ok(WasapiEvent::Volume(self.get_volume())),
                WaitResult::Object(1 | 4) | WaitResult::Timeout => {
                    let connected = self.connect();
                    return Ok(WasapiEvent::Connected(connected));
                }
                WaitResult::Object(_) => unreachable!(),
            }
        }
    }

    pub fn acquire(&mut self) -> Result<Option<OutputBuffer<'_>>, WasapiError> {
        if !self.active {
            return Ok(None);
        }
        let Some(connection) = &mut self.connection else {
            return Ok(None);
        };
        connection.acquire().context(AcquireBufferSnafu)
    }

    pub fn is_empty(&self) -> Result<bool, WasapiError> {
        match &self.connection {
            Some(connection) => connection.is_empty().context(EmptySnafu),
            None => Ok(true),
        }
    }

    pub fn get_volume(&mut self) -> f32 {
        if let Some(connection) = &self.connection
            && let Ok(volume) = connection.get_volume()
        {
            self.volume = Some(volume);
        }

        self.volume.unwrap_or(1.0)
    }

    pub fn set_active(&mut self, active: bool) -> bool {
        if active == self.active {
            return self.connection.is_some();
        }
        self.active = active;
        let Some(connection) = &mut self.connection else {
            return false;
        };
        if let Err(e) = connection.set_active(active) {
            warn!("{e}");
            return false;
        }
        true
    }

    pub fn set_volume(&mut self, volume: f32) -> bool {
        self.volume = Some(volume);
        let Some(connection) = &self.connection else {
            return false;
        };
        if let Err(e) = connection.set_volume(volume) {
            warn!("{e}");
            return false;
        }
        true
    }

    pub fn set_rate(&mut self, rate: u32) -> bool {
        self.rate = rate;
        self.connect()
    }

    pub fn reset(&mut self) -> bool {
        let Some(connection) = &mut self.connection else {
            return false;
        };
        if let Err(e) = connection.reset(self.active) {
            warn!("{e}");
            return false;
        }
        true
    }

    fn disconnect(&mut self) {
        self.connection = None;
        if let Some(manager) = &self.device_manager {
            manager.set_active_device(None);
        }
    }

    fn schedule_retry(&mut self) {
        self.retry_delay = self
            .retry_delay
            .saturating_mul(2)
            .min(Duration::from_secs(5));
    }
}

impl StreamEvents {
    fn new() -> Result<Self, SyncError> {
        Ok(Self {
            device: WakeEvent::new()?,
            audio: WakeEvent::new()?,
            volume: WakeEvent::new()?,
            disconnect: WakeEvent::new()?,
        })
    }
}
