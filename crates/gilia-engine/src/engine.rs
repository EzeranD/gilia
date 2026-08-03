// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    sync::{Arc, LazyLock, Mutex, atomic::AtomicI64},
    thread,
    time::Instant,
};

use arc_swap::ArcSwap;
use crossbeam_channel::Sender;
use tracing::error;

use crate::{
    TrackKind,
    session::{ActiveTracks, PlayerEvent, PlayerMeta, PlayerSession, SessionState, TrackId},
    subtitle::decoder::SubtitleFrame,
    utils::Clock,
    video::frame::VideoFrame,
};

static ENGINE_START: LazyLock<Instant> = LazyLock::new(Instant::now);

pub fn get_engine_start() -> &'static Instant {
    &ENGINE_START
}

#[derive(Debug, Clone)]
pub enum ExternalEvent {
    Opened(PlayerMeta),
    NewFrame,
    TrackChanged(TrackKind, TrackId),
    VolumesChanged(Vec<f32>),
    Eof,
    Error(EngineError),
}

pub type ExternalCallback = Arc<dyn Fn(ExternalEvent) + Send + Sync>;

pub struct PlayerEngine {
    pub config: Arc<EngineConfig>,
    pub state: Arc<ArcSwap<SessionState>>,
    pub clock: Arc<Clock>,
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
    pub clock: Arc<Clock>,
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
        ffmpeg_next::init().unwrap();
        ffmpeg_next::log::set_level(ffmpeg_next::log::Level::Quiet);

        get_engine_start();
        let external_callback = Arc::new(callback);
        Self {
            config: Arc::new(config),
            state: Arc::new(ArcSwap::from_pointee(SessionState::new(ActiveTracks {
                audio: None,
                video: None,
                subs: None,
            }))),
            clock: Arc::new(Clock::default()),
            external_callback,
            event_tx: None,
            audio_info: AudioInfo {
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
            clock: self.clock.clone(),
            audio_info: self.audio_info.clone(),
            video_output: self.video_output.clone(),
            sub_output: self.sub_output.clone(),
        };
        let external_callback = self.external_callback.clone();
        let event_tx_clone = event_tx.clone();

        let _ = thread::Builder::new()
            .name("session".into())
            .spawn(move || {
                match PlayerSession::open(
                    &path,
                    shared,
                    &external_callback,
                    event_tx_clone,
                    session_state.clone(),
                ) {
                    Ok(mut session) => {
                        external_callback(ExternalEvent::Opened(session.meta.clone()));
                        while let Ok(event) = event_rx.recv() {
                            session.apply_event(event, &event_rx);
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

    pub fn active_tracks(&self) -> ActiveTracks {
        self.state.load().tracks
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
            renderer.set_frame_size(widget_width.round() as i32, widget_height.round() as i32);
            renderer.set_margins(
                margin.1.round() as i32,
                margin.1.round() as i32,
                margin.0.round() as i32,
                margin.0.round() as i32,
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

    pub fn position_ms(&self) -> i64 {
        self.clock.get_ms()
    }
}
