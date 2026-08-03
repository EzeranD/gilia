// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{marker::PhantomData, rc::Rc};

use snafu::{ResultExt, Snafu};
use windows::{
    Win32::{
        Foundation::HANDLE,
        System::{
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
            },
            Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW},
        },
    },
    core::{GUID, Interface, PCWSTR},
};

#[derive(Debug, Snafu)]
pub enum ComError {
    #[snafu(display("failed to initialize COM"))]
    InitCom { source: windows::core::Error },
    #[snafu(display("failed to create a COM instance"))]
    CreateComInstance { source: windows::core::Error },
    #[snafu(display("failed to register with MMCSS"))]
    RegisterMmcss { source: windows::core::Error },
}

pub struct Com(PhantomData<Rc<()>>);

pub struct MmcssRegistration(HANDLE);

pub(crate) fn pcwstr_to_string(s: &PCWSTR) -> Option<String> {
    if s.is_null() {
        return None;
    }
    unsafe { s.to_string().ok() }
}

impl Com {
    pub fn init() -> Result<Self, ComError> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .context(InitComSnafu)?;
        };
        Ok(Self(PhantomData))
    }

    pub fn create_instance<T: Interface>(clsid: &GUID) -> Result<T, ComError> {
        unsafe { CoCreateInstance(clsid, None, CLSCTX_ALL).context(CreateComInstanceSnafu) }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

impl MmcssRegistration {
    pub fn register(task_name: PCWSTR) -> Result<Self, ComError> {
        let mut task_index = 0;
        let handle = unsafe {
            AvSetMmThreadCharacteristicsW(task_name, &raw mut task_index)
                .context(RegisterMmcssSnafu)?
        };
        Ok(Self(handle))
    }
}

impl Drop for MmcssRegistration {
    fn drop(&mut self) {
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}
