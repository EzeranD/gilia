use core::slice;
use std::{
    cell::RefCell,
    ffi::CString,
    path::Path,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Instant,
};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use ffmpeg_next::{
    Packet, Stream,
    codec::Context,
    format::{context::Input, input},
    media::Type,
};
use libass::{OverrideBits, Style};
use tracing::{debug, error};

use crate::{
    PlayerEvent::Internal,
    audio::{
        self,
        decoder::AudioDecoder,
        queue::{ThreadWaker, WakingSender, channel, queue},
    },
    core::{
        AbEffect, ActiveStreams, DecoderEffect, DemuxerEffect, InternalEvent::FramesDrained,
        PlaybackMode, PlaybackPhase, PlayerCore, PlayerEvent, PlayerSnapshot,
    },
    demuxer::Demuxer,
    subtitle::decoder::{SubtitleDecoder, SubtitleFrame},
    sync::Clock,
    video::{decoder::VideoDecoder, frame::VideoFrame},
};

static ENGINE_START: LazyLock<Instant> = LazyLock::new(Instant::now);

pub fn get_engine_start() -> &'static Instant {
    &ENGINE_START
}

#[derive(Debug, Clone)]
pub enum ExternalEvent {
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
    video_output: VideoOutput,
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

struct VideoOutput {
    frame_rx: Option<Receiver<VideoFrame>>,
    frame: RefCell<Option<VideoFrame>>,
    next_frame: Arc<Mutex<Option<VideoFrame>>>,
    drain: Arc<AtomicBool>,
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
            video_output: VideoOutput {
                frame_rx: None,
                frame: RefCell::new(None),
                next_frame: Arc::new(Mutex::new(None)),
                drain: Arc::new(AtomicBool::new(false)),
            },
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
            channels.frame_rx = Some(frame_rx.clone());
            channels.next_frame = Some(self.video_output.next_frame.clone());
            self.video_output.frame_rx = Some(frame_rx);

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

    pub fn tick_playback(&self) -> (bool, u64) {
        let Some(audio_clock) = &self.audio_info.audio_clock else {
            return (false, 1);
        };
        let Some(frame_rx) = &self.video_output.frame_rx else {
            return (false, 1);
        };

        const THRESHOLD: i64 = 20;
        let audio_ms = audio_clock.get_ms();
        let mut current_frame = {
            if let Some(f) = self.video_output.next_frame.lock().unwrap().take() {
                f
            } else {
                match frame_rx.try_recv() {
                    Ok(f) => f,
                    Err(TryRecvError::Empty) => {
                        if matches!(self.state.load().phase, PlaybackPhase::Draining(_)) {
                            self.apply_event(Internal(FramesDrained));
                            return (false, 1);
                        }
                        return (false, 1);
                    }
                    Err(_) => return (false, 1),
                }
            }
        };

        loop {
            let current_pts = current_frame.info.pts.unwrap();
            let av_offset = current_pts - audio_ms;
            debug!("audio ms: {audio_ms}, av_offset: {av_offset}");

            if av_offset > THRESHOLD {
                *self.video_output.next_frame.lock().unwrap() = Some(current_frame);
                return (false, (av_offset - THRESHOLD) as u64);
            }

            if av_offset < -THRESHOLD {
                match frame_rx.try_recv() {
                    Ok(f) => {
                        current_frame = f;
                        continue;
                    }
                    Err(TryRecvError::Empty) => {
                        if self.video_output.drain.load(Ordering::Relaxed) {
                            self.apply_event(Internal(FramesDrained));
                            return (false, 1);
                        }
                        return (false, 1);
                    }
                    Err(_) => {
                        *self.video_output.frame.borrow_mut() = Some(current_frame);
                        return (true, 1);
                    }
                }
            }

            let next_frame = {
                match frame_rx.try_recv() {
                    Ok(f) => f,
                    Err(TryRecvError::Empty) => {
                        if self.video_output.drain.load(Ordering::Relaxed) {
                            self.apply_event(Internal(FramesDrained));
                            return (false, 1);
                        }
                        return (false, 1);
                    }
                    Err(_) => {
                        *self.video_output.frame.borrow_mut() = Some(current_frame);
                        return (true, 1);
                    }
                }
            };
            let next_error = next_frame.info.pts.unwrap() - audio_ms;
            if next_error <= THRESHOLD {
                current_frame = next_frame;
            } else {
                *self.video_output.next_frame.lock().unwrap() = Some(next_frame);
                *self.video_output.frame.borrow_mut() = Some(current_frame);
                return (true, next_error as u64);
            }
        }
    }

    pub fn set_callback(&mut self, callback: ExternalCallback) {
        self.external_callback = Some(callback);
    }

    pub fn frame(&self) -> Option<VideoFrame> {
        self.video_output.frame.borrow().clone()
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
    pub audio_tx: Option<WakingSender>,
    pub audio_packet_tx: Option<Sender<Packet>>,
    pub pw_tx: Option<pipewire::channel::Sender<AbEffect>>,
    pub video_tx: Option<Sender<DecoderEffect>>,
    pub video_packet_tx: Option<Sender<Packet>>,
    pub sub_tx: Option<Sender<DecoderEffect>>,
    pub sub_packet_tx: Option<Sender<Packet>>,
    pub frame_rx: Option<Receiver<VideoFrame>>,
    pub next_frame: Option<Arc<Mutex<Option<VideoFrame>>>>,
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
            video_tx: None,
            video_packet_tx: None,
            sub_tx: None,
            sub_packet_tx: None,
            frame_rx: None,
            next_frame: None,
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

    pub fn flush_frames(&self) {
        if let Some(rx) = &self.frame_rx {
            while rx.try_recv().is_ok() {}
        }
        if let Some(next) = &self.next_frame {
            *next.lock().unwrap() = None;
        }
    }
}
