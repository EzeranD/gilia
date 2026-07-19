use core::slice;
use std::{
    cell::RefCell,
    ffi::CString,
    path::Path,
    sync::{Arc, LazyLock, Mutex, atomic::AtomicI64},
    thread,
    time::Instant,
};

use arc_swap::ArcSwap;
use crossbeam_channel::Sender;
use ffmpeg_next::{
    Packet, Stream,
    codec::Context,
    format::{context::Input, input},
    media::Type,
};
use libass::{OverrideBits, Style};
use tracing::error;

use crate::{
    audio::{self, decoder::AudioDecoder, queue::queue},
    core::{
        AbEffect, ActiveStreams, DecoderEffect, DemuxerEffect, PlaybackMode, PlaybackPhase,
        PlayerCore, PlayerEvent, PlayerSnapshot, VoEffect,
    },
    demuxer::Demuxer,
    subtitle::decoder::{SubtitleDecoder, SubtitleFrame},
    sync::Clock,
    utils::{ThreadWaker, WakingSender, channel},
    video::{decoder::VideoDecoder, frame::VideoFrame, output::VideoOutput},
};

static ENGINE_START: LazyLock<Instant> = LazyLock::new(Instant::now);

pub fn get_engine_start() -> &'static Instant {
    &ENGINE_START
}

#[derive(Debug, Clone)]
pub enum ExternalEvent {
    NewFrame,
    VolumesChanged(Vec<f32>),
    Eof,
}

pub type ExternalCallback = Arc<dyn Fn(ExternalEvent) + Send + Sync>;

pub struct PlayerEngine {
    pub config: Arc<EngineConfig>,
    pub state: Arc<ArcSwap<PlayerSnapshot>>,
    external_callback: Option<ExternalCallback>,
    event_tx: Option<Sender<PlayerEvent>>,
    pub audio_info: AudioInfo,
    frame: Arc<Mutex<Option<VideoFrame>>>,
    pub current_pts: Arc<AtomicI64>,
    sub_output: SubOutput,
}

#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    pub hw_dec: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("failed to open input file")]
    InputOpenError(#[from] ffmpeg_next::Error),
}

pub struct AudioInfo {
    audio_clock: Option<Arc<Clock>>,
    pub volume: Arc<Mutex<Vec<f32>>>,
}

struct SubOutput {
    renderer: Option<RefCell<libass::Renderer>>,
    track: Option<Arc<Mutex<libass::Track>>>,
}

impl PlayerEngine {
    #[must_use]
    pub fn new(config: EngineConfig) -> Self {
        get_engine_start();
        Self {
            config: Arc::new(config),
            state: Arc::new(ArcSwap::from_pointee(PlayerSnapshot {
                mode: PlaybackMode::Playing,
                phase: PlaybackPhase::Normal,
            })),
            external_callback: None,
            event_tx: None,
            audio_info: AudioInfo {
                audio_clock: None,
                volume: Arc::new(Mutex::new(Vec::new())),
            },
            frame: Arc::new(Mutex::new(None)),
            current_pts: Arc::new(AtomicI64::new(0)),
            sub_output: SubOutput {
                renderer: None,
                track: None,
            },
        }
    }
    pub fn open(&mut self, path: String) -> Result<(), EngineError> {
        let ictx = input(&path)?;

        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        self.event_tx = Some(event_tx.clone());
        let mut channels = PlayerChannels::new();
        channels.external_callback = self.external_callback.clone();

        let audio_stream = get_stream(&ictx, Type::Audio);
        let video_stream = get_stream(&ictx, Type::Video);
        let sub_stream = get_stream(&ictx, Type::Subtitle);

        let mut audio_idx = None;
        let mut video_idx = None;
        let mut sub_idx = None;

        if let Some(audio_stream) = audio_stream {
            audio_idx = Some(audio_stream.1);

            let (audio_packet_tx, audio_packet_rx) = crossbeam_channel::bounded(50);
            let (pw_tx, pw_rx) = pipewire::channel::channel();

            channels.audio_packet_tx = Some(audio_packet_tx);
            channels.pw_tx = Some(pw_tx);

            let audio_clock = Arc::new(Clock::default());
            channels.audio_clock = Some(audio_clock.clone());
            self.audio_info.audio_clock = Some(audio_clock.clone());

            let audio_context = Context::from_parameters(audio_stream.0.parameters()).unwrap();
            let audio_time_base = audio_stream.0.time_base();
            let decoder = audio_context.decoder().audio().unwrap();
            let audio_rate = decoder.rate();
            let waker = Arc::new(ThreadWaker::new());
            let (audio_tx, audio_rx) = channel(waker.clone());
            channels.audio_tx = Some(audio_tx);
            let (audio_buffer_tx, audio_buffer_rx) = queue(128, waker.clone());
            let audio_event_tx = event_tx.clone();
            let decoder_waker = waker.clone();
            std::thread::Builder::new()
                .name("audio-decoder".into())
                .spawn(move || {
                    decoder_waker.set();
                    let mut audio_decoder = AudioDecoder::new(
                        decoder,
                        audio_packet_rx,
                        audio_buffer_tx,
                        audio_time_base,
                        audio_rx,
                        audio_event_tx,
                    );
                    audio_decoder.process();
                })
                .unwrap();
            let ab_event_tx = event_tx.clone();
            audio::ab_pipewire::spawn(
                audio_buffer_rx,
                pw_rx,
                ab_event_tx,
                audio_rate,
                audio_clock.clone(),
                self.audio_info.volume.clone(),
                self.external_callback.clone(),
                path,
            );
        }

        if let Some(video_stream) = video_stream {
            video_idx = Some(video_stream.1);
            let (frame_tx, frame_rx) = crossbeam_channel::bounded(3);
            let (video_tx, video_rx) = crossbeam_channel::unbounded();
            let (video_packet_tx, video_packet_rx) = crossbeam_channel::bounded(3);

            channels.video_tx = Some(video_tx);
            channels.video_packet_tx = Some(video_packet_tx);

            let config = self.config.clone();
            let video_time_base = video_stream.0.time_base();
            let parameters = video_stream.0.parameters();
            let video_event_tx = event_tx.clone();
            std::thread::Builder::new()
                .name("video-decoder".into())
                .spawn(move || {
                    let mut decoder = VideoDecoder::new(
                        config,
                        parameters,
                        video_packet_rx,
                        frame_tx,
                        video_time_base,
                        video_rx,
                        video_event_tx,
                    );
                    decoder.process();
                })
                .unwrap();

            let waker = Arc::new(ThreadWaker::new());
            let (vp_tx, vp_rx) = crossbeam_channel::unbounded();
            let sender = WakingSender::new(vp_tx, waker.clone());
            channels.video_playback_tx = Some(sender);

            let mut tick_scheduler = VideoOutput::new(
                self.audio_info.audio_clock.clone(),
                frame_rx.clone(),
                self.frame.clone(),
                self.current_pts.clone(),
                vp_rx,
                event_tx.clone(),
                self.external_callback.clone(),
            );
            std::thread::spawn(move || {
                waker.set();
                tick_scheduler.process();
            });
        }

        if let Some(sub_stream) = sub_stream {
            sub_idx = Some(sub_stream.1);
            let sub_context = Context::from_parameters(sub_stream.0.parameters()).unwrap();
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
                                let data = slice::from_raw_parts(
                                    (*ptr).extradata,
                                    (*ptr).extradata_size as usize,
                                );
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
                    let data =
                        slice::from_raw_parts((*ptr).extradata, (*ptr).extradata_size as usize);
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
            let sub_time_base = sub_stream.0.time_base();
            let shared_track = Arc::new(Mutex::new(track));
            self.sub_output.track = Some(shared_track.clone());
            self.sub_output.renderer = Some(RefCell::new(renderer));
            let sub_track = shared_track.clone();
            let (sub_packet_tx, sub_packet_rx) = crossbeam_channel::bounded(50);
            let (sub_tx, sub_rx) = crossbeam_channel::unbounded();
            channels.sub_tx = Some(sub_tx);
            channels.sub_packet_tx = Some(sub_packet_tx);
            let sub_event_tx = event_tx.clone();
            std::thread::Builder::new()
                .name("sub-decoder".into())
                .spawn(move || {
                    let mut sub_decoder = SubtitleDecoder::new(
                        sub_context,
                        sub_packet_rx,
                        sub_track,
                        sub_time_base,
                        sub_rx,
                        sub_event_tx,
                    );
                    sub_decoder.process();
                })
                .unwrap();
        }

        let demuxer_event_tx = event_tx.clone();
        let (demuxer_tx, demuxer_rx) = crossbeam_channel::unbounded();
        channels.demuxer_tx = Some(demuxer_tx);
        let mut demuxer = Demuxer::new(
            ictx,
            video_idx,
            audio_idx,
            sub_idx,
            channels.video_packet_tx.clone(),
            channels.audio_packet_tx.clone(),
            channels.sub_packet_tx.clone(),
            demuxer_rx,
            demuxer_event_tx,
        );
        std::thread::Builder::new()
            .name("demuxer".into())
            .spawn(move || {
                demuxer.read_packets();
            })
            .unwrap();

        let core_state = self.state.clone();
        thread::Builder::new()
            .name("core".into())
            .spawn(move || {
                let streams = ActiveStreams {
                    subs: sub_idx.is_some(),
                    audio: audio_idx.is_some(),
                    video: video_idx.is_some(),
                };
                let mut core = PlayerCore::new(streams);
                while let Ok(event) = event_rx.recv() {
                    core.apply_event(event, &channels);
                    core_state.store(Arc::new(core.snapshot()));
                }
            })
            .unwrap();

        Ok(())
    }

    pub fn apply_event(&self, event: PlayerEvent) {
        if let Some(tx) = &self.event_tx
            && let Err(e) = tx.send(event)
        {
            error!("Failed to send event: {}", e);
        }
    }

    pub fn set_callback(&mut self, callback: ExternalCallback) {
        self.external_callback = Some(callback);
    }

    pub fn frame(&self) -> Option<VideoFrame> {
        self.frame.lock().unwrap().clone()
    }

    pub fn update_viewport(&self, widget_width: f32, widget_height: f32, scale: [f32; 2]) {
        if let Some(renderer) = &self.sub_output.renderer {
            let scaled = (widget_width * scale[0], widget_height * scale[1]);
            let margin = (
                (widget_width - scaled.0) / 2.0,
                (widget_height - scaled.1) / 2.0,
            );
            let mut renderer = renderer.borrow_mut();
            renderer.set_frame_size(widget_width as i32, widget_height as i32);
            renderer.set_margins(
                margin.1 as i32,
                margin.1 as i32,
                margin.0 as i32,
                margin.0 as i32,
            );
        }
    }

    pub fn current_subs(&self, frame_pts: i64) -> Option<SubtitleFrame> {
        let (Some(renderer), Some(track)) = (&self.sub_output.renderer, &self.sub_output.track)
        else {
            return None;
        };
        let mut renderer = renderer.borrow_mut();
        let (images, change) = renderer.render_frame(&mut track.lock().unwrap(), frame_pts);
        Some(SubtitleFrame {
            layers: images.into_iter().flatten().collect(),
            change,
        })
    }

    pub fn position_ms(&self) -> Option<i64> {
        let audio_clock = self.audio_info.audio_clock.as_ref()?;
        Some(audio_clock.get_ms())
    }
}

pub fn get_stream(ictx: &Input, stream_type: Type) -> Option<(Stream<'_>, usize)> {
    let stream = ictx.streams().best(stream_type)?;
    let stream_idx = stream.index();
    Some((stream, stream_idx))
}

pub struct PlayerChannels {
    pub demuxer_tx: Option<Sender<DemuxerEffect>>,
    pub audio_tx: Option<WakingSender<DecoderEffect>>,
    pub audio_packet_tx: Option<Sender<Packet>>,
    pub pw_tx: Option<pipewire::channel::Sender<AbEffect>>,
    pub video_tx: Option<Sender<DecoderEffect>>,
    pub video_packet_tx: Option<Sender<Packet>>,
    pub video_playback_tx: Option<WakingSender<VoEffect>>,
    pub sub_tx: Option<Sender<DecoderEffect>>,
    pub sub_packet_tx: Option<Sender<Packet>>,
    pub external_callback: Option<ExternalCallback>,
    pub audio_clock: Option<Arc<Clock>>,
}

impl PlayerChannels {
    fn new() -> Self {
        Self {
            demuxer_tx: None,
            audio_tx: None,
            audio_packet_tx: None,
            pw_tx: None,
            video_playback_tx: None,
            video_tx: None,
            video_packet_tx: None,
            sub_tx: None,
            sub_packet_tx: None,
            external_callback: None,
            audio_clock: None,
        }
    }

    pub fn ab(&self, effect: AbEffect) {
        if let Some(tx) = &self.pw_tx {
            let _ = tx.send(effect);
        }
    }
    pub fn demuxer(&self, effect: DemuxerEffect) {
        if let Some(tx) = &self.demuxer_tx {
            let _ = tx.send(effect);
        }
    }
    pub fn audio(&self, effect: DecoderEffect) {
        if let Some(tx) = &self.audio_tx {
            let _ = tx.send(effect);
        }
    }
    pub fn video(&self, effect: DecoderEffect) {
        if let Some(tx) = &self.video_tx {
            let _ = tx.send(effect);
        }
    }
    pub fn sub(&self, effect: DecoderEffect) {
        if let Some(tx) = &self.sub_tx {
            let _ = tx.send(effect);
        }
    }
    pub fn vp(&self, effect: VoEffect) {
        if let Some(tx) = &self.video_playback_tx {
            let _ = tx.send(effect);
        }
    }
}
