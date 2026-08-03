// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    ffi::{CStr, c_int},
    mem::ManuallyDrop,
    ptr,
    sync::{Arc, atomic::AtomicBool},
};

use ffmpeg_next::{
    Codec,
    codec::{Context, Parameters},
    decoder::Video,
    ffi::{AVCodec, AVCodecContext, AVCodecHWConfig, AVPixelFormat},
    frame, sys as ffmsys,
};
use tracing::{debug, error, trace};

pub struct ManagedVideo {
    decoder: ManuallyDrop<Video>,
}

#[derive(Debug)]
pub struct HwInfo {
    pub fmt: AVPixelFormat,
    pub initialized: Arc<AtomicBool>,
}

pub struct BufferRef(pub *mut ffmsys::AVBufferRef);

pub struct HwOption {
    pub hw_dev_ctx: BufferRef,
    pub codec: *const ffmsys::AVCodec,
    pub hw_cfg: *const ffmsys::AVCodecHWConfig,
}

struct HwConfigIter<'a> {
    codec: &'a ffmsys::AVCodec,
    index: c_int,
}

pub fn get_hw_options(parameters: &Parameters) -> Vec<HwOption> {
    let mut hw_options = Vec::new();
    let mut hw_device = ffmsys::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;

    loop {
        hw_device = unsafe { ffmsys::av_hwdevice_iterate_types(hw_device) };

        if hw_device == ffmsys::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            break;
        }

        #[cfg(target_os = "linux")]
        if !matches!(
            hw_device,
            ffmsys::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI
                | ffmsys::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN
        ) {
            continue;
        }

        #[cfg(target_os = "windows")]
        if !matches!(hw_device, ffmsys::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN) {
            continue;
        }

        let Ok(hw_device_ctx) = create_hw_device_ctx(hw_device).inspect_err(|e| debug!("{e}"))
        else {
            continue;
        };

        let mut iter = std::ptr::null_mut();
        loop {
            let codec = unsafe { ffmsys::av_codec_iterate(&raw mut iter) };
            if codec.is_null() {
                break;
            }
            unsafe {
                if (*codec).id != (*parameters.as_ptr()).codec_id {
                    continue;
                }

                if ffmsys::av_codec_is_decoder(codec) == 0 {
                    continue;
                }

                for hw_config in HwConfigIter::new(&*codec).filter(|c| c.device_type == hw_device) {
                    hw_options.push(HwOption {
                        hw_dev_ctx: hw_device_ctx.clone(),
                        codec,
                        hw_cfg: hw_config,
                    });
                }
            }
        }
    }
    hw_options
}

pub fn create_decoder(
    parameters: &Parameters,
    initialized: Arc<AtomicBool>,
    hw_option: &HwOption,
) -> ManagedVideo {
    unsafe {
        let mut ctx = Context::new_with_codec(Codec::wrap(hw_option.codec.cast_mut()));

        let _ = ctx.set_parameters(parameters.clone());

        let hw_info = Box::new(HwInfo {
            fmt: (*hw_option.hw_cfg).pix_fmt,
            initialized,
        });

        (*ctx.as_mut_ptr()).get_format = Some(get_format);
        (*ctx.as_mut_ptr()).hw_device_ctx = ffmsys::av_buffer_ref(hw_option.hw_dev_ctx.0);
        (*ctx.as_mut_ptr()).opaque = Box::into_raw(hw_info).cast();

        let video_decoder = ctx
            .decoder()
            .video()
            .inspect_err(|e| error!("Video decoder failed to start {e}"))
            .unwrap();

        ManagedVideo::new(video_decoder)
    }
}

pub fn hwframe_map(hw_format: AVPixelFormat, frame: &frame::Video) -> Result<frame::Video, String> {
    let mut hw_frame = frame::Video::empty();
    unsafe {
        (*hw_frame.as_mut_ptr()).format = hw_format as i32;
    };
    let ret = unsafe {
        ffmsys::av_hwframe_map(
            hw_frame.as_mut_ptr(),
            frame.as_ptr(),
            ffmsys::AV_HWFRAME_MAP_READ as i32,
        )
    };
    if ret < 0 {
        error!("Transfer failed: {ret}");
        return Err(format!("Transfer failed: {ret}"));
    }
    Ok(hw_frame)
}

pub fn create_hw_device_ctx(hw_device: ffmsys::AVHWDeviceType) -> Result<BufferRef, String> {
    let mut hw_device_ctx: *mut ffmsys::AVBufferRef = std::ptr::null_mut();
    let res = unsafe {
        ffmsys::av_hwdevice_ctx_create(
            &raw mut hw_device_ctx,
            hw_device,
            ptr::null(),
            ptr::null_mut(),
            0,
        )
    };
    let type_name = unsafe { CStr::from_ptr(ffmsys::av_hwdevice_get_type_name(hw_device)) };
    if res == 0 {
        Ok(BufferRef(hw_device_ctx))
    } else {
        Err(format!("{type_name:?} failed: {res}"))
    }
}

pub unsafe extern "C" fn get_format(
    avctx: *mut AVCodecContext,
    mut fmt: *const AVPixelFormat,
) -> AVPixelFormat {
    unsafe {
        let info: &mut HwInfo = &mut *((*avctx).opaque.cast());
        while (*fmt) != AVPixelFormat::AV_PIX_FMT_NONE {
            trace!("Trying format: {:?}", *fmt);

            if (*fmt) == info.fmt {
                let device_ref = (*avctx).hw_device_ctx;
                let mut hw_frames_ctx: *mut ffmsys::AVBufferRef = std::ptr::null_mut();
                let res = ffmsys::avcodec_get_hw_frames_parameters(
                    avctx,
                    device_ref,
                    *fmt,
                    &raw mut hw_frames_ctx,
                );
                if res == 0 {
                    let res = ffmsys::av_hwframe_ctx_init(hw_frames_ctx);
                    if res == 0 {
                        (*avctx).hw_frames_ctx = hw_frames_ctx;
                        info.initialized
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        return *fmt;
                    }
                }
            }
            fmt = fmt.add(1);
        }
        AVPixelFormat::AV_PIX_FMT_NONE
    }
}

impl ManagedVideo {
    pub fn new(decoder: Video) -> Self {
        Self {
            decoder: ManuallyDrop::new(decoder),
        }
    }

    pub fn send_packet(&mut self, packet: &ffmpeg_next::Packet) -> Result<(), ffmpeg_next::Error> {
        self.decoder.send_packet(packet)
    }

    pub fn receive_frame(
        &mut self,
        frame: &mut ffmpeg_next::frame::Video,
    ) -> Result<(), ffmpeg_next::Error> {
        self.decoder.receive_frame(frame)
    }

    pub fn skip_frame(&mut self, discard: ffmpeg_next::Discard) {
        self.decoder.skip_frame(discard);
    }

    pub fn flush(&mut self) {
        self.decoder.flush();
    }
}

impl Drop for ManagedVideo {
    fn drop(&mut self) {
        unsafe {
            let ctx_ptr = self.decoder.as_mut_ptr();
            let info: *mut HwInfo = (*ctx_ptr).opaque.cast();

            ManuallyDrop::drop(&mut self.decoder);

            if !info.is_null() {
                drop(Box::from_raw(info));
            }
        }
    }
}

impl Clone for BufferRef {
    fn clone(&self) -> Self {
        unsafe { BufferRef(ffmsys::av_buffer_ref(self.0)) }
    }
}

impl Drop for BufferRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                ffmsys::av_buffer_unref(&raw mut self.0);
            }
        }
    }
}

impl std::fmt::Debug for BufferRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BufferRef({:?})", self.0)
    }
}

impl std::fmt::Debug for HwOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HwOption")
            .field("hw_dev_ctx", &self.hw_dev_ctx)
            .field("codec", &self.codec)
            .field("hw_cfg", &self.hw_cfg)
            .finish()
    }
}

impl<'a> HwConfigIter<'a> {
    pub fn new(codec: &'a AVCodec) -> Self {
        Self { codec, index: 0 }
    }
}

impl<'a> Iterator for HwConfigIter<'a> {
    type Item = &'a AVCodecHWConfig;

    fn next(&mut self) -> Option<Self::Item> {
        let hw_config = unsafe { ffmsys::avcodec_get_hw_config(self.codec, self.index) };
        if hw_config.is_null() {
            return None;
        }
        self.index += 1;
        Some(unsafe { &*hw_config })
    }
}
