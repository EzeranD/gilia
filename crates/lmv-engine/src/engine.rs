use std::{
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    thread,
    time::Instant,
};

use arc_swap::ArcSwap;
use crossbeam_channel::Sender;
use ffmpeg_next::{Packet, Stream, format::context::Input, media::Type};
use tracing::error;

use crate::{
    session::{
        AbEffect, DecoderEffect, DemuxerEffect, PlaybackMode, PlaybackPhase, PlayerEvent,
        PlayerSession, PlayerSnapshot, VoEffect,
    },
    subtitle::decoder::SubtitleFrame,
    utils::{Clock, WakingSender},
    video::frame::VideoFrame,
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
    Error(EngineError),
}

pub type ExternalCallback = Arc<dyn Fn(ExternalEvent) + Send + Sync>;

pub struct PlayerEngine {
    pub config: Arc<EngineConfig>,
    pub state: Arc<ArcSwap<PlayerSnapshot>>,
    external_callback: ExternalCallback,
    event_tx: Option<Sender<PlayerEvent>>,
    pub audio_info: AudioInfo,
    pub video_output: VideoOutput,
    sub_output: SubOutput,
}

#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    pub hw_dec: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum EngineError {
    #[error("failed to open input file")]
    InputOpenError(#[from] ffmpeg_next::Error),
}

#[derive(Clone)]
pub struct AudioInfo {
    pub audio_clock: Arc<Clock>,
    pub volume: Arc<Mutex<Vec<f32>>>,
}

#[derive(Clone)]
pub struct VideoOutput {
    pub current_pts: Arc<AtomicI64>,
    pub(crate) frame: Arc<Mutex<Option<VideoFrame>>>,
}

#[derive(Clone)]
pub struct SubOutput {
    pub renderer: Arc<Mutex<Option<libass::Renderer>>>,
    pub track: Arc<Mutex<Option<libass::Track>>>,
}

#[derive(Clone)]
pub struct SharedPlayerState {
    pub config: Arc<EngineConfig>,
    pub audio_info: AudioInfo,
    pub video_output: VideoOutput,
    pub sub_output: SubOutput,
}

impl PlayerEngine {
    #[must_use]
    pub fn new<F>(config: EngineConfig, callback: F) -> Self
    where
        F: Fn(ExternalEvent) + Send + Sync + 'static,
    {
        get_engine_start();
        let external_callback = Arc::new(callback);
        Self {
            config: Arc::new(config),
            state: Arc::new(ArcSwap::from_pointee(PlayerSnapshot {
                mode: PlaybackMode::Playing,
                phase: PlaybackPhase::Normal,
            })),
            external_callback,
            event_tx: None,
            audio_info: AudioInfo {
                audio_clock: Arc::new(Clock::default()),
                volume: Arc::new(Mutex::new(Vec::new())),
            },
            video_output: VideoOutput {
                current_pts: Arc::new(AtomicI64::new(0)),
                frame: Arc::new(Mutex::new(None)),
            },
            sub_output: SubOutput {
                renderer: Arc::new(Mutex::new(None)),
                track: Arc::new(Mutex::new(None)),
            },
        }
    }
    pub fn open(&mut self, path: String) -> Result<(), EngineError> {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        self.event_tx = Some(event_tx.clone());
        let session_state = self.state.clone();

        let shared = SharedPlayerState {
            config: self.config.clone(),
            audio_info: self.audio_info.clone(),
            video_output: self.video_output.clone(),
            sub_output: self.sub_output.clone(),
        };
        let external_callback = self.external_callback.clone();
        let event_tx_clone = event_tx.clone();

        let _ = thread::Builder::new()
            .name("session".into())
            .spawn(move || {
                match PlayerSession::open(&path, shared, &external_callback, event_tx_clone) {
                    Ok(mut session) => {
                        while let Ok(event) = event_rx.recv() {
                            session.apply_event(event);
                            session_state.store(Arc::new(session.snapshot()));
                        }
                    }
                    Err(e) => {
                        error!("Failed to open a playback session for '{}': {:?}", path, e);
                        external_callback(ExternalEvent::Error(e));
                    }
                }
            });

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
        self.external_callback = callback;
    }

    pub fn frame(&self) -> Option<VideoFrame> {
        self.video_output.frame.lock().unwrap().clone()
    }

    pub fn update_viewport(&self, widget_width: f32, widget_height: f32, scale: [f32; 2]) {
        let mut renderer_guard = self.sub_output.renderer.lock().unwrap();
        if let Some(renderer) = &mut *renderer_guard {
            let scaled = (widget_width * scale[0], widget_height * scale[1]);
            let margin = (
                (widget_width - scaled.0) / 2.0,
                (widget_height - scaled.1) / 2.0,
            );
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
        let mut renderer_guard = self.sub_output.renderer.lock().unwrap();
        let mut track_guard = self.sub_output.track.lock().unwrap();
        let (Some(renderer), Some(track)) = (&mut *renderer_guard, &mut *track_guard) else {
            return None;
        };
        let (images, change) = renderer.render_frame(track, frame_pts);
        Some(SubtitleFrame {
            layers: images.into_iter().flatten().collect(),
            change,
        })
    }

    pub fn position_ms(&self) -> Option<i64> {
        if !self.audio_info.audio_clock.active.load(Ordering::Relaxed) {
            return None;
        }
        Some(self.audio_info.audio_clock.get_ms())
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
    pub video_output_tx: Option<Sender<VoEffect>>,
    pub sub_tx: Option<Sender<DecoderEffect>>,
    pub sub_packet_tx: Option<Sender<Packet>>,
    pub external_callback: Option<ExternalCallback>,
    pub audio_clock: Option<Arc<Clock>>,
}

impl PlayerChannels {
    pub fn new() -> Self {
        Self {
            demuxer_tx: None,
            audio_tx: None,
            audio_packet_tx: None,
            pw_tx: None,
            video_output_tx: None,
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
    pub fn vo(&self, effect: VoEffect) {
        if let Some(tx) = &self.video_output_tx {
            let _ = tx.send(effect);
        }
    }
}
