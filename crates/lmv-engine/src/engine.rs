use std::{
    sync::{Arc, LazyLock, Mutex, atomic::AtomicI64},
    thread,
    time::Instant,
};

use arc_swap::ArcSwap;
use crossbeam_channel::Sender;
use ffmpeg_next::{
    Packet, Stream,
    format::{context::Input, input},
    media::Type,
};
use tracing::error;

use crate::{
    audio::spawn_audio_stream,
    core::{
        AbEffect, ActiveStreams, DecoderEffect, DemuxerEffect, PlaybackMode, PlaybackPhase,
        PlayerCore, PlayerEvent, PlayerSnapshot, VoEffect,
    },
    demuxer::Demuxer,
    subtitle::{decoder::SubtitleFrame, spawn_sub_stream},
    sync::Clock,
    utils::WakingSender,
    video::{frame::VideoFrame, spawn_video_stream},
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
    renderer: Option<Arc<Mutex<libass::Renderer>>>,
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
            let (dec_effect_tx, pw_tx, packet_tx, clock) = spawn_audio_stream(
                &audio_stream,
                &event_tx,
                self.external_callback.as_ref(),
                &self.audio_info.volume,
                path,
            );

            channels.audio_tx = Some(dec_effect_tx);
            channels.pw_tx = Some(pw_tx);
            channels.audio_packet_tx = Some(packet_tx);
            channels.audio_clock = Some(clock.clone());
            self.audio_info.audio_clock = Some(clock);
        }

        if let Some(video_stream) = video_stream {
            video_idx = Some(video_stream.1);
            let (dec_tx, vo_tx, packet_tx) = spawn_video_stream(
                &video_stream,
                &event_tx,
                self.external_callback.as_ref(),
                &self.config,
                &self.frame,
                &self.current_pts,
                self.audio_info.audio_clock.as_ref(),
            );
            channels.video_tx = Some(dec_tx);
            channels.video_output_tx = Some(vo_tx);
            channels.video_packet_tx = Some(packet_tx);
        }

        if let Some(sub_stream) = sub_stream {
            sub_idx = Some(sub_stream.1);
            let (sub_tx, packet_tx, track, renderer) =
                spawn_sub_stream(&ictx, &event_tx, sub_stream);
            channels.sub_tx = Some(sub_tx);
            channels.sub_packet_tx = Some(packet_tx);
            self.sub_output.track = Some(track);
            self.sub_output.renderer = Some(Arc::new(Mutex::new(renderer)));
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
            let mut renderer = renderer.lock().unwrap();
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
        let mut renderer = renderer.lock().unwrap();
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
    pub video_output_tx: Option<WakingSender<VoEffect>>,
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
    pub fn vp(&self, effect: VoEffect) {
        if let Some(tx) = &self.video_output_tx {
            let _ = tx.send(effect);
        }
    }
}
