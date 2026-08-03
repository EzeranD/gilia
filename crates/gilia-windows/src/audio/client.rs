// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::slice;

use snafu::{ResultExt, Snafu};
use windows::{
    Win32::{
        Media::{
            Audio::{
                AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                AudioSessionDisconnectReason, AudioSessionState, IAudioClient, IAudioClock,
                IAudioRenderClient, IAudioSessionControl, IAudioSessionEvents,
                IAudioSessionEvents_Impl, ISimpleAudioVolume, WAVEFORMATEX,
            },
            Multimedia::WAVE_FORMAT_IEEE_FLOAT,
        },
        System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    },
    core::{BOOL, GUID, PCWSTR, implement},
};

use super::device::{AudioDevice, DeviceError};
use crate::sync::WakeEvent;

const CHANNELS: u16 = 2;
const SAMPLE_BYTES: usize = std::mem::size_of::<f32>();
const SAMPLE_BITS: u16 = (SAMPLE_BYTES * 8) as u16;

#[derive(Debug, Snafu)]
pub enum ClientError {
    #[snafu(display("failed to activate device"))]
    ActivateDevice { source: DeviceError },
    #[snafu(display("failed to initialize client"))]
    InitAudioClient { source: windows::core::Error },
    #[snafu(display("failed to set event handle on client"))]
    SetEventHandle { source: windows::core::Error },
    #[snafu(display("failed to get client service"))]
    GetService { source: windows::core::Error },
    #[snafu(display("failed to get buffer size"))]
    GetBufferSize { source: windows::core::Error },
    #[snafu(display("failed to get master volume"))]
    GetVolume { source: windows::core::Error },
    #[snafu(display("failed to set master volume"))]
    SetVolume { source: windows::core::Error },
    #[snafu(display("failed to start client"))]
    Start { source: windows::core::Error },
    #[snafu(display("failed to stop client"))]
    Stop { source: windows::core::Error },
    #[snafu(display("failed to get current padding"))]
    GetPadding { source: windows::core::Error },
    #[snafu(display("failed to reset client"))]
    Reset { source: windows::core::Error },
    #[snafu(display("buffer size is too large"))]
    InvalidBufferSize,
    #[snafu(display("failed to get buffer"))]
    GetBuffer { source: windows::core::Error },
    #[snafu(display("returned buffer pointer was null"))]
    InvalidBuffer,
    #[snafu(display("failed to release buffer"))]
    ReleaseBuffer { source: windows::core::Error },
    #[snafu(display("failed to register session notification"))]
    RegisterSession { source: windows::core::Error },
    #[snafu(display("failed to get clock frequency"))]
    GetClockFrequency { source: windows::core::Error },
    #[snafu(display("failed to query performance frequency"))]
    QueryPerformanceFrequency { source: windows::core::Error },
    #[snafu(display("failed to get clock position"))]
    GetClockPosition { source: windows::core::Error },
    #[snafu(display("failed to query performance counter"))]
    QueryPerformanceCounter { source: windows::core::Error },
}

pub(super) struct AudioClient {
    _session: SessionControl,
    render: RenderClient,
    volume: ISimpleAudioVolume,
    clock: AudioClock,
    inner: IAudioClient,
    rate: u32,
    frame_bytes: usize,
    buffer_frames: u32,
}

pub struct OutputBuffer<'a> {
    render: &'a mut RenderClient,
    data: *mut u8,
    frames: usize,
    frame_bytes: usize,
    byte_len: usize,
    timing: OutputTiming,
    release_attempted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputTiming {
    pub rate: u32,
    pub delay_ns: i64,
}

struct RenderClient {
    inner: IAudioRenderClient,
    frames_submitted: u64,
}

struct SessionControl {
    inner: IAudioSessionControl,
    events: IAudioSessionEvents,
}

struct AudioClock {
    inner: IAudioClock,
    frequency: u64,
    qpc_frequency: i64,
}

#[implement(IAudioSessionEvents)]
struct SessionEvents {
    volume_event: WakeEvent,
    disconnect_event: WakeEvent,
}

impl AudioClient {
    pub(super) fn new(
        device: &AudioDevice,
        rate: u32,
        volume: Option<f32>,
        active: bool,
        audio_event: &WakeEvent,
        volume_event: &WakeEvent,
        disconnect_event: &WakeEvent,
    ) -> Result<(Self, f32), ClientError> {
        let inner = device.activate().context(ActivateDeviceSnafu)?;

        let block_align = CHANNELS * SAMPLE_BYTES as u16;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: SAMPLE_BITS,
            cbSize: 0,
        };
        let frame_bytes = format.nBlockAlign as usize;

        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

        unsafe {
            inner
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    0,
                    0,
                    &raw const format,
                    Option::None,
                )
                .context(InitAudioClientSnafu)?;

            inner
                .SetEventHandle(audio_event.raw())
                .context(SetEventHandleSnafu)?;
        }

        let render = RenderClient::new(unsafe { inner.GetService().context(GetServiceSnafu)? });

        let volume_service: ISimpleAudioVolume =
            unsafe { inner.GetService().context(GetServiceSnafu)? };
        let clock = AudioClock::new(unsafe { inner.GetService().context(GetServiceSnafu)? })?;
        let buffer_frames = unsafe { inner.GetBufferSize().context(GetBufferSizeSnafu)? };

        let volume = match volume {
            Some(vol) => {
                unsafe {
                    volume_service
                        .SetMasterVolume(vol.clamp(0.0, 1.0), std::ptr::null())
                        .context(SetVolumeSnafu)?;
                }
                vol
            }
            None => unsafe { volume_service.GetMasterVolume().context(GetVolumeSnafu)? },
        };

        let session = SessionControl::new(
            unsafe { inner.GetService().context(GetServiceSnafu)? },
            volume_event,
            disconnect_event,
        )?;

        let client = Self {
            _session: session,
            render,
            volume: volume_service,
            clock,
            inner,
            rate,
            frame_bytes,
            buffer_frames,
        };

        if active {
            client.set_active(true)?;
        }

        Ok((client, volume))
    }

    pub(super) fn get_volume(&self) -> Result<f32, ClientError> {
        unsafe { self.volume.GetMasterVolume().context(GetVolumeSnafu) }
    }

    pub(super) fn set_volume(&self, volume: f32) -> Result<(), ClientError> {
        unsafe {
            self.volume
                .SetMasterVolume(volume.clamp(0.0, 1.0), std::ptr::null())
                .context(SetVolumeSnafu)
        }
    }

    pub(super) fn set_active(&self, active: bool) -> Result<(), ClientError> {
        if active {
            unsafe { self.inner.Start().context(StartSnafu) }
        } else {
            unsafe { self.inner.Stop().context(StopSnafu) }
        }
    }

    pub(super) fn acquire(&mut self) -> Result<Option<OutputBuffer<'_>>, ClientError> {
        let padding = unsafe { self.inner.GetCurrentPadding().context(GetPaddingSnafu)? };
        let needed = self.buffer_frames.saturating_sub(padding);
        if needed == 0 {
            return Ok(None);
        }

        let delay_ns = self
            .clock
            .delay_ns(self.render.frames_submitted, self.rate)?;
        let timing = OutputTiming {
            rate: self.rate,
            delay_ns,
        };

        let buffer = self.render.acquire(needed, self.frame_bytes, timing)?;
        Ok(Some(buffer))
    }

    pub(super) fn is_empty(&self) -> Result<bool, ClientError> {
        Ok(unsafe { self.inner.GetCurrentPadding().context(GetPaddingSnafu)? } == 0)
    }

    pub(super) fn reset(&mut self, active: bool) -> Result<(), ClientError> {
        unsafe { self.inner.Stop().context(StopSnafu)? };
        unsafe { self.inner.Reset().context(ResetSnafu)? };
        self.render.frames_submitted = 0;
        if active {
            unsafe { self.inner.Start().context(StartSnafu)? };
        }
        Ok(())
    }
}

impl OutputBuffer<'_> {
    pub fn timing(&self) -> OutputTiming {
        self.timing
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.data, self.byte_len) }
    }

    pub fn commit(mut self, filled_frames: usize) -> Result<(), ClientError> {
        let filled_bytes = filled_frames.min(self.frames) * self.frame_bytes;
        self.data_mut()[filled_bytes..].fill(0);
        let frames = self.frames as u32;
        self.release_attempted = true;
        self.render.release_buffer(frames)?;
        self.render.frames_submitted += frames as u64;
        Ok(())
    }
}

impl Drop for OutputBuffer<'_> {
    fn drop(&mut self) {
        if !self.release_attempted {
            let _ = self.render.release_buffer(0);
        }
    }
}

impl RenderClient {
    fn new(inner: IAudioRenderClient) -> Self {
        Self {
            inner,
            frames_submitted: 0,
        }
    }

    fn acquire(
        &mut self,
        frames: u32,
        frame_bytes: usize,
        timing: OutputTiming,
    ) -> Result<OutputBuffer<'_>, ClientError> {
        let byte_len = (frames as usize)
            .checked_mul(frame_bytes)
            .filter(|&len| isize::try_from(len).is_ok())
            .ok_or(ClientError::InvalidBufferSize)?;

        let data = unsafe { self.inner.GetBuffer(frames).context(GetBufferSnafu)? };
        if data.is_null() {
            let _ = self.release_buffer(0);
            return InvalidBufferSnafu.fail();
        }

        Ok(OutputBuffer {
            render: self,
            data,
            frames: frames as usize,
            frame_bytes,
            byte_len,
            timing,
            release_attempted: false,
        })
    }

    fn release_buffer(&self, frames: u32) -> Result<(), ClientError> {
        unsafe {
            self.inner
                .ReleaseBuffer(frames, 0)
                .context(ReleaseBufferSnafu)
        }
    }
}

impl SessionControl {
    fn new(
        inner: IAudioSessionControl,
        volume_event: &WakeEvent,
        disconnect_event: &WakeEvent,
    ) -> Result<Self, ClientError> {
        let events: IAudioSessionEvents = SessionEvents {
            volume_event: volume_event.clone(),
            disconnect_event: disconnect_event.clone(),
        }
        .into();

        unsafe {
            inner
                .RegisterAudioSessionNotification(&events)
                .context(RegisterSessionSnafu)?;
        }

        Ok(Self { inner, events })
    }
}

impl Drop for SessionControl {
    fn drop(&mut self) {
        let _ = unsafe { self.inner.UnregisterAudioSessionNotification(&self.events) };
    }
}

impl AudioClock {
    fn new(inner: IAudioClock) -> Result<Self, ClientError> {
        let frequency = unsafe { inner.GetFrequency().context(GetClockFrequencySnafu)? };

        let mut qpc_frequency = 0i64;
        unsafe {
            QueryPerformanceFrequency(&raw mut qpc_frequency)
                .context(QueryPerformanceFrequencySnafu)?;
        }

        Ok(Self {
            inner,
            frequency,
            qpc_frequency,
        })
    }

    fn delay_ns(&self, frames_submitted: u64, rate: u32) -> Result<i64, ClientError> {
        let mut position = 0u64;
        let mut qpc_position = 0u64;
        unsafe {
            self.inner
                .GetPosition(&raw mut position, Some(&raw mut qpc_position))
                .context(GetClockPositionSnafu)?;
        }

        let mut qpc_now = 0i64;
        unsafe {
            QueryPerformanceCounter(&raw mut qpc_now).context(QueryPerformanceCounterSnafu)?;
        }

        let submitted_ns = (frames_submitted as i128 * 1_000_000_000 / rate as i128) as i64;
        let device_ns = (position as i128 * 1_000_000_000 / self.frequency as i128) as i64;
        let qpc_now_100ns = (qpc_now as i128 * 10_000_000 / self.qpc_frequency as i128) as i64;
        let mut qpc_elapsed_ns = (qpc_now_100ns - qpc_position as i64) * 100;
        if qpc_elapsed_ns > 10_000_000_000 || qpc_elapsed_ns < 0 {
            qpc_elapsed_ns = 0;
        }
        Ok((submitted_ns - device_ns - qpc_elapsed_ns).max(0))
    }
}

impl IAudioSessionEvents_Impl for SessionEvents_Impl {
    fn OnDisplayNameChanged(
        &self,
        _new_display_name: &PCWSTR,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnIconPathChanged(
        &self,
        _new_icon_path: &PCWSTR,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnSimpleVolumeChanged(
        &self,
        _new_volume: f32,
        _new_mute: BOOL,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        let _ = self.volume_event.signal();
        Ok(())
    }

    fn OnChannelVolumeChanged(
        &self,
        _channel_count: u32,
        _new_channel_volume_array: *const f32,
        _changed_channel: u32,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnGroupingParamChanged(
        &self,
        _new_grouping_param: *const GUID,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnStateChanged(&self, _new_state: AudioSessionState) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnSessionDisconnected(
        &self,
        _disconnect_reason: AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        let _ = self.disconnect_event.signal();
        Ok(())
    }
}
