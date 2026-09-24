// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    ffi::c_void,
    os::fd::{BorrowedFd, OwnedFd},
    sync::Arc,
};

use ffmpeg_next::{
    color::Space, ffi as ffmsys, ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME, format::Pixel, frame,
};
use tracing::{debug, warn};

use crate::video::hw_ffmpeg;

#[derive(Debug, Clone, Copy)]
pub enum DecoderMode {
    Hw,
    Sw,
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub info: FrameInfo,
    pub frame: Frame,
}

#[derive(Clone)]
pub enum Frame {
    Hw(GpuFrame, Arc<frame::Video>),
    Sw(Arc<frame::Video>),
}

#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u32,
    pub multiplier: f32,
    pub pts: Option<i64>,
    pub limited: bool,
    pub is_packed: bool,
    pub color_matrix: [[f32; 4]; 3],
    pub log2_chroma_w: u8,
    pub log2_chroma_h: u8,
}

#[derive(Debug, Clone)]
pub enum GpuFrame {
    Vaapi { planes: Vec<GpuPlane> },
    Vulkan { images: [ash::vk::Image; 8] },
}

#[derive(Debug)]
pub struct GpuPlane {
    pub fd: OwnedFd,
    pub format: u32,
    pub format_modifier: u64,
    pub offset: isize,
    pub pitch: isize,
}

#[repr(C)]
struct AVVkFrame {
    img: [ash::vk::Image; 8],
    tiling: ash::vk::ImageTiling,
    mem: [ash::vk::DeviceMemory; 8],
    size: [usize; 8],
    flags: ash::vk::MemoryPropertyFlags,
    access: [ash::vk::AccessFlags2; 8],
    layout: [ash::vk::ImageLayout; 8],
    sem: [ash::vk::Semaphore; 8],
    sem_value: [u64; 8],
    offset: [isize; 8],
    queue_family: [u32; 8],
    internal: *mut c_void,
}

#[derive(Debug)]
pub struct Coeffs {
    coeff0: f32,
    coeff1: f32,
    coeff2: f32,
    coeff3: f32,
}

pub fn wrap_frame(
    frame: frame::Video,
    pts: Option<i64>,
    mode: DecoderMode,
    printed: &mut bool,
) -> VideoFrame {
    if !*printed {
        debug!("--- Video Frame Probe ---");
        debug!("Resolution:  {}x{}", frame.width(), frame.height());
        debug!("Format:      {:?}", frame.format());
        debug!("Color Range: {:?}", frame.color_range());
        debug!("Color Space: {:?}", frame.color_space());
        debug!("Interlaced:  {:?}", frame.is_interlaced());
        debug!("YUV Weights: {:?}", get_weights(&frame));
        debug!("-------------------------");
        *printed = true;
    }

    match mode {
        DecoderMode::Sw => handle_sw_frame(frame, pts),
        DecoderMode::Hw => handle_hw_frame(frame, pts),
    }
}

fn handle_sw_frame(frame: frame::Video, pts: Option<i64>) -> VideoFrame {
    let limited = match frame.color_range() {
        ffmpeg_next::color::Range::JPEG => false,
        ffmpeg_next::color::Range::MPEG | ffmpeg_next::color::Range::Unspecified => true,
    };
    let (log2_chroma_w, log2_chroma_h, shift, bit_depth, is_packed) = unsafe {
        let desc = ffmsys::av_pix_fmt_desc_get(frame.format().into());
        let log2_chroma_w = (*desc).log2_chroma_w as u8;
        let log2_chroma_h = (*desc).log2_chroma_h as u8;
        let shift = (*desc).comp[0].shift as u32;
        let depth = (*desc).comp[0].depth as u32;
        let packed = (*desc).comp[1].plane == (*desc).comp[2].plane;
        (log2_chroma_w, log2_chroma_h, shift, depth, packed)
    };

    let t = if bit_depth > 8 { 16 } else { 8 };
    let container_max = (1u32 << t) - 1;
    let content_max = (1u32 << bit_depth) - 1;
    let shift_mult = 1u32 << shift;
    let multiplier = container_max as f32 / (content_max as f32 * shift_mult as f32);
    let info = FrameInfo {
        width: frame.width(),
        height: frame.height(),
        pts,
        bit_depth,
        multiplier,
        limited,
        is_packed,
        color_matrix: get_matrix(&frame, bit_depth),
        log2_chroma_w,
        log2_chroma_h,
    };
    VideoFrame {
        info,
        frame: Frame::Sw(Arc::new(frame)),
    }
}

fn handle_hw_frame(frame: frame::Video, pts: Option<i64>) -> VideoFrame {
    let format = frame.format();
    let gpu_frame = match format {
        Pixel::VAAPI => {
            let hw_frame = hw_ffmpeg::hwframe_map(AV_PIX_FMT_DRM_PRIME, &frame).unwrap();
            let mut planes = Vec::new();
            let frame_ptr = unsafe { *hw_frame.as_ptr() };
            let desc_ptr = frame_ptr.data[0] as *const ffmsys::AVDRMFrameDescriptor;
            let desc = unsafe { &*desc_ptr };
            for layer in &desc.layers[0..desc.nb_layers as usize] {
                for plane in &layer.planes[0..layer.nb_planes as usize] {
                    let object = desc.objects[plane.object_index as usize];
                    let borrowed = unsafe { BorrowedFd::borrow_raw(object.fd) };
                    let fd = borrowed.try_clone_to_owned().unwrap();
                    planes.push(GpuPlane {
                        fd,
                        format: layer.format,
                        format_modifier: object.format_modifier,
                        offset: plane.offset,
                        pitch: plane.pitch,
                    });
                }
            }
            GpuFrame::Vaapi { planes }
        }
        Pixel::VULKAN => {
            let data: *mut AVVkFrame = unsafe {
                let frame_ptr = frame.as_ptr();
                (*frame_ptr).data[0].cast()
            };
            GpuFrame::Vulkan {
                images: unsafe { (*data).img },
            }
        }
        _ => todo!("Unsupported pixel format: {format:?}"),
    };

    let limited = match frame.color_range() {
        ffmpeg_next::color::Range::JPEG => false,
        ffmpeg_next::color::Range::MPEG | ffmpeg_next::color::Range::Unspecified => true,
    };
    let (log2_chroma_w, log2_chroma_h, shift, bit_depth, is_packed) = unsafe {
        let frame_ptr = frame.as_ptr();
        let hw_frames_ctx = (*frame_ptr).hw_frames_ctx;
        let frames_ctx = (*hw_frames_ctx).data as *const ffmsys::AVHWFramesContext;
        let sw_format = (*frames_ctx).sw_format;
        let desc = ffmsys::av_pix_fmt_desc_get(sw_format);
        let log2_chroma_w = (*desc).log2_chroma_w as u8;
        let log2_chroma_h = (*desc).log2_chroma_h as u8;
        let shift = (*desc).comp[0].shift as u32;
        let depth = (*desc).comp[0].depth as u32;
        let packed = (*desc).comp[1].plane == (*desc).comp[2].plane;
        (log2_chroma_w, log2_chroma_h, shift, depth, packed)
    };

    let t = if bit_depth > 8 { 16 } else { 8 };
    let container_max = (1u32 << t) - 1;
    let content_max = (1u32 << bit_depth) - 1;
    let shift_mult = 1u32 << shift;
    let multiplier = container_max as f32 / (content_max as f32 * shift_mult as f32);
    let info = FrameInfo {
        width: frame.width(),
        height: frame.height(),
        pts,
        bit_depth,
        multiplier,
        limited,
        is_packed,
        color_matrix: get_matrix(&frame, bit_depth),
        log2_chroma_w,
        log2_chroma_h,
    };
    VideoFrame {
        info,
        frame: Frame::Hw(gpu_frame, Arc::new(frame)),
    }
}

#[must_use]
pub fn get_weights(video: &frame::Video) -> Coeffs {
    match video.color_space() {
        Space::BT709 => Coeffs::new(0.2126, 0.0722),
        Space::BT470BG | Space::SMPTE170M => Coeffs::new(0.299, 0.114),
        Space::Unspecified => {
            if video.width() >= 1280 || video.height() > 576 {
                Coeffs::new(0.2126, 0.0722)
            } else {
                Coeffs::new(0.299, 0.114)
            }
        }
        Space::BT2020NCL => Coeffs::new(0.2627, 0.0593),
        space => {
            warn!("Color space not supported: {space:?}",);
            if video.width() >= 1280 || video.height() > 576 {
                Coeffs::new(0.2126, 0.0722)
            } else {
                Coeffs::new(0.299, 0.114)
            }
        }
    }
}

pub fn get_matrix(video: &frame::Video, depth: u32) -> [[f32; 4]; 3] {
    let coeffs = get_weights(video);
    let limited = match video.color_range() {
        ffmpeg_next::color::Range::JPEG => false,
        ffmpeg_next::color::Range::MPEG | ffmpeg_next::color::Range::Unspecified => true,
    };
    // TODO clean this up I just want to be done here
    let max_value = 2_u32.pow(depth) - 1;
    let scale = 2_u32.pow(depth - 8);
    let y_range = if limited {
        219.0 * scale as f32
    } else {
        max_value as f32
    };
    let uv_range = if limited {
        224.0 * scale as f32
    } else {
        max_value as f32
    };
    let uv_mid = 128.0 * scale as f32;
    let y_weight = 1.0 / y_range;
    let y_off = if limited { -16.0 / 219.0 } else { 0.0 };
    let red = [
        y_weight * max_value as f32,
        0.0 * max_value as f32,
        (coeffs.coeff0 / uv_range) * max_value as f32,
        y_off - coeffs.coeff0 * uv_mid / uv_range,
    ];

    let green = [
        y_weight * max_value as f32,
        (-coeffs.coeff2 / uv_range) * max_value as f32,
        (-coeffs.coeff3 / uv_range) * max_value as f32,
        y_off + (coeffs.coeff2 + coeffs.coeff3) * uv_mid / uv_range,
    ];

    let blue = [
        y_weight * max_value as f32,
        (coeffs.coeff1 / uv_range) * max_value as f32,
        0.0 * max_value as f32,
        y_off - coeffs.coeff1 * uv_mid / uv_range,
    ];
    [red, green, blue]
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Frame::Hw(frame, _) => write!(f, "Hw({frame:?})"),
            Frame::Sw(_) => write!(f, "Sw(..)"),
        }
    }
}

impl Clone for GpuPlane {
    fn clone(&self) -> Self {
        Self {
            fd: self.fd.try_clone().expect("failed to dup fd"),
            format: self.format,
            format_modifier: self.format_modifier,
            offset: self.offset,
            pitch: self.pitch,
        }
    }
}

impl Coeffs {
    fn new(kr: f32, kb: f32) -> Self {
        let kg = 1.0 - kr - kb;

        Self {
            coeff0: 2.0 * (1.0 - kr),
            coeff1: 2.0 * (1.0 - kb),
            coeff2: 2.0 * kb * (1.0 - kb) / kg,
            coeff3: 2.0 * kr * (1.0 - kr) / kg,
        }
    }
}
