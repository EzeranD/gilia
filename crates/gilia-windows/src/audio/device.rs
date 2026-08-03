// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use snafu::{ResultExt, Snafu, ensure};
use tracing::warn;
use windows::{
    Win32::{
        Foundation::PROPERTYKEY,
        Media::Audio::{
            DEVICE_STATE, DEVICE_STATE_DISABLED, DEVICE_STATE_NOTPRESENT, DEVICE_STATE_UNPLUGGED,
            EDataFlow, ERole, IAudioClient, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
            IMMNotificationClient_Impl, MMDeviceEnumerator, PKEY_AudioEngine_DeviceFormat,
            eMultimedia, eRender,
        },
        System::Com::{CLSCTX_ALL, CoTaskMemFree},
    },
    core::{PCWSTR, implement},
};

use crate::{
    com::{Com, ComError, pcwstr_to_string},
    sync::WakeEvent,
};

#[derive(Debug, Snafu)]
pub enum DeviceError {
    #[snafu(display("failed to create device enumerator"))]
    CreateEnumerator { source: ComError },
    #[snafu(display("failed to register endpoint notification callback"))]
    RegisterNotification { source: windows::core::Error },
    #[snafu(display("failed to get default endpoint"))]
    DefaultEndpoint { source: windows::core::Error },
    #[snafu(display("failed to get device id"))]
    DeviceId { source: windows::core::Error },
    #[snafu(display("device id was null"))]
    NullDeviceId,
    #[snafu(display("failed to decode device id"))]
    DecodeDeviceId { source: std::string::FromUtf16Error },
    #[snafu(display("failed to unregister endpoint notification callback"))]
    UnregisterNotification { source: windows::core::Error },
    #[snafu(display("failed to activate device"))]
    ActivateDevice { source: windows::core::Error },
}

pub(super) struct DeviceManager {
    enumerator: IMMDeviceEnumerator,
    notification: IMMNotificationClient,
    active_id: Arc<Mutex<Option<String>>>,
}

pub(super) struct AudioDevice {
    inner: IMMDevice,
}

#[implement(IMMNotificationClient)]
struct DeviceNotification {
    wake_event: WakeEvent,
    active_id: Arc<Mutex<Option<String>>>,
}

impl DeviceManager {
    pub(super) fn new(wake_event: WakeEvent) -> Result<Self, DeviceError> {
        let enumerator: IMMDeviceEnumerator =
            Com::create_instance(&MMDeviceEnumerator).context(CreateEnumeratorSnafu)?;
        let active_id = Arc::new(Mutex::new(None));
        let notification: IMMNotificationClient = DeviceNotification {
            wake_event,
            active_id: active_id.clone(),
        }
        .into();

        unsafe {
            enumerator
                .RegisterEndpointNotificationCallback(&notification)
                .context(RegisterNotificationSnafu)?;
        }

        Ok(Self {
            enumerator,
            notification,
            active_id,
        })
    }

    pub(super) fn default_device(&self) -> Result<AudioDevice, DeviceError> {
        let inner = unsafe {
            self.enumerator
                .GetDefaultAudioEndpoint(eRender, eMultimedia)
                .context(DefaultEndpointSnafu)?
        };

        let id_ptr = unsafe { inner.GetId().context(DeviceIdSnafu)? };
        ensure!(!id_ptr.is_null(), NullDeviceIdSnafu);

        let id = unsafe { id_ptr.to_string() };
        unsafe { CoTaskMemFree(Some(id_ptr.as_ptr().cast())) };
        let id = id.context(DecodeDeviceIdSnafu)?;

        *self.active_id.lock().unwrap() = Some(id);
        Ok(AudioDevice { inner })
    }

    pub(super) fn set_active_device(&self, id: Option<String>) {
        *self.active_id.lock().unwrap() = id;
    }
}

impl Drop for DeviceManager {
    fn drop(&mut self) {
        if let Err(e) = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.notification)
                .context(UnregisterNotificationSnafu)
        } {
            warn!("{e}");
        }
    }
}

impl AudioDevice {
    pub(super) fn activate(&self) -> Result<IAudioClient, DeviceError> {
        unsafe {
            self.inner
                .Activate(CLSCTX_ALL, None)
                .context(ActivateDeviceSnafu)
        }
    }
}

impl DeviceNotification {
    fn is_active_device(&self, device_id: &PCWSTR) -> bool {
        let Some(device_id) = pcwstr_to_string(device_id) else {
            return false;
        };
        self.active_id.lock().unwrap().as_deref() == Some(device_id.as_str())
    }
}

impl IMMNotificationClient_Impl for DeviceNotification_Impl {
    fn OnDeviceStateChanged(
        &self,
        device_id: &PCWSTR,
        new_state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        if self.is_active_device(device_id)
            && matches!(
                new_state,
                DEVICE_STATE_DISABLED | DEVICE_STATE_NOTPRESENT | DEVICE_STATE_UNPLUGGED
            )
        {
            let _ = self.wake_event.signal();
        }
        Ok(())
    }

    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        default_device_id: &PCWSTR,
    ) -> windows::core::Result<()> {
        if flow == eRender && role == eMultimedia {
            let new_id = pcwstr_to_string(default_device_id);
            let active_id = self.active_id.lock().unwrap();
            if *active_id != new_id {
                let _ = self.wake_event.signal();
            }
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        device_id: &PCWSTR,
        key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        if self.is_active_device(device_id) && *key == PKEY_AudioEngine_DeviceFormat {
            let _ = self.wake_event.signal();
        }
        Ok(())
    }
}
