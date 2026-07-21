use std::{
    ffi::CString,
    path::Path,
    slice,
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use crossbeam_channel::Sender;
use ffmpeg_next::{Stream, codec::Context, format::context::Input, media::Type};
use libass::{OverrideBits, Renderer, Style, Track};

use crate::{
    session::{DecoderEffect, PlayerEvent},
    subtitle::decoder::SubtitleDecoder,
    utils::{Clock, TrackedPacket},
};

pub mod decoder;

pub fn spawn_sub_stream(
    ictx: &Input,
    event_tx: &Sender<PlayerEvent>,
    sub_stream: &Stream,
    clock: Arc<Clock>,
    shared_track: Arc<Mutex<Option<Track>>>,
    shared_renderer: Arc<Mutex<Option<Renderer>>>,
) -> (JoinHandle<()>, Sender<DecoderEffect>, Sender<TrackedPacket>) {
    let sub_context = Context::from_parameters(sub_stream.parameters()).unwrap();
    let mut lib = libass::Library::new().unwrap();
    for stream in ictx.streams() {
        if stream.parameters().medium() == Type::Attachment {
            let metadata = stream.metadata();
            let Some(filename) = metadata.get("filename") else {
                continue;
            };
            let lower = filename.to_lowercase();
            let ext = Path::new(&lower).extension();
            if matches!(
                ext.and_then(|e| e.to_str()),
                Some("ttf" | "otf" | "ttc" | "otc" | "pfb" | "pfm")
            ) {
                let attach_context = Context::from_parameters(stream.parameters()).unwrap();
                unsafe {
                    let ptr = attach_context.as_ptr();
                    if !(*ptr).extradata.is_null() && (*ptr).extradata_size > 0 {
                        let data =
                            slice::from_raw_parts((*ptr).extradata, (*ptr).extradata_size as usize);
                        lib.add_font(filename, data);
                    }
                }
            }
        }
    }
    let mut renderer = libass::Renderer::new(&mut lib).unwrap();
    renderer.set_margins(0, 0, 0, 0);
    renderer.use_margins(false);
    renderer.set_fonts(
        None,
        "sans-serif",
        libass::DefaultFontProvider::Autodetect,
        None,
        true,
    );
    let mut track = lib.new_track().unwrap();
    unsafe {
        let ptr = sub_context.as_ptr();
        if !(*ptr).extradata.is_null() && (*ptr).extradata_size > 0 {
            let data = slice::from_raw_parts((*ptr).extradata, (*ptr).extradata_size as usize);
            track.process_codec_private(data);
        } else {
            let header = "\
                [Events]\n\
                Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";
            track.process_codec_private(header.as_bytes());

            renderer.set_selective_style_override(&Style {
                name: CString::new("Default").unwrap(),
                font_name: CString::new("Ghandi Sans").unwrap(),
                font_size: 15.0,
                primary_color: 0xFFFF_FF00,
                secondary_color: 0xFFFF_FF00,
                outline_color: 0x0000_0000,
                back_color: 0x0000_0000,
                bold: false,
                italic: false,
                underline: false,
                strikeout: false,
                scale_x: 1.0,
                scale_y: 1.0,
                spacing: 0.0,
                angle: 0.0,
                border_style: 1,
                outline: 1.0,
                shadow: 0.0,
                alignment: 2,
                margin_l: 10,
                margin_r: 10,
                margin_v: 10,
                encoding: 1,
                treat_fontname_as_pattern: true,
                blur: 0.0,
                justify: 0,
            });
            renderer.set_selective_style_override_enabled(OverrideBits::FULL_STYLE);
        }
    }
    let sub_time_base = sub_stream.time_base();
    let sub_track = shared_track.clone();
    *shared_renderer.lock().unwrap() = Some(renderer);
    *shared_track.lock().unwrap() = Some(track);

    let (sub_packet_tx, sub_packet_rx) = crossbeam_channel::unbounded();
    let (sub_tx, sub_rx) = crossbeam_channel::unbounded();
    let sub_event_tx = event_tx.clone();
    let dec_handle = std::thread::Builder::new()
        .name("sub-decoder".into())
        .spawn(move || {
            let mut sub_decoder = SubtitleDecoder::new(
                sub_context,
                sub_packet_rx,
                sub_track,
                lib,
                sub_time_base,
                sub_rx,
                sub_event_tx,
                clock,
            );
            sub_decoder.process();
        })
        .unwrap();
    (dec_handle, sub_tx, sub_packet_tx)
}
