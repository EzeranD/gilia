// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    sync::{Arc, LazyLock, Mutex, atomic::AtomicI64},
    thread,
    time::Instant,
};

use arc_swap::ArcSwap;
use tracing::error;

use crate::{
    channel::{self, Sender},
    subtitle::decoder::SubtitleFrame,
    utils::Clock,
    video::frame::VideoFrame,
};

mod messages;
mod metadata;
mod open;
mod state;
mod worker;

pub(crate) use messages::SessionChannels;
pub use messages::{
    AbEffect, ControlEvent, DecoderEffect, DemuxerEffect, ExternalEvent, InternalEvent,
    PlayerEvent, ReinitDetails, SyncMode, VoEffect,
};
pub use metadata::{ChapterMeta, PlayerMeta, TrackId, TrackKind, TrackMeta};
pub use state::{
    ActiveTracks, DrainingState, PlaybackMode, PlaybackOperation, PlaybackPhase, SeekingState,
    SessionState, TrackDrainState, TrackSeekState, Worker,
};
pub use worker::PlayerThreads;
use worker::SessionWorker;

pub type Pts = state::Pts;

pub struct Session {
    config: Arc<SessionConfig>,
    pub state: Arc<ArcSwap<SessionState>>,
    pub clock: Arc<Clock>,
    external_callback: ExternalCallback,
    event_tx: Option<Sender<PlayerEvent>>,
    audio_info: AudioInfo,
    pub video_output: VideoOutput,
    sub_output: SubOutput,
}

#[derive(Clone)]
pub struct SharedPlayerState {
    pub config: Arc<SessionConfig>,
    pub clock: Arc<Clock>,
    pub audio_info: AudioInfo,
    pub video_output: VideoOutput,
    pub sub_output: SubOutput,
}

#[derive(Debug, Clone, Copy)]
pub struct SessionConfig {
    pub hw_dec: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SessionError {
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

pub type ExternalCallback = Arc<dyn Fn(ExternalEvent) + Send + Sync>;

static SESSION_START: LazyLock<Instant> = LazyLock::new(Instant::now);

pub fn get_session_start() -> &'static Instant {
    &SESSION_START
}

impl Session {
    #[must_use]
    pub fn new<F>(config: SessionConfig, callback: F) -> Self
    where
        F: Fn(ExternalEvent) + Send + Sync + 'static,
    {
        ffmpeg_next::init().unwrap();
        ffmpeg_next::log::set_level(ffmpeg_next::log::Level::Quiet);

        get_session_start();
        let external_callback = Arc::new(callback);

        Self {
            config: Arc::new(config),
            state: Arc::new(ArcSwap::from_pointee(SessionState::new())),
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

    pub fn open(&mut self, path: String) -> Result<(), SessionError> {
        let (event_tx, event_rx) = channel::Builder::unbounded().channel();
        self.event_tx = Some(event_tx.clone());
        self.state.store(Arc::new(SessionState::new()));
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
                match SessionWorker::open(
                    &path,
                    shared,
                    &external_callback,
                    event_tx_clone,
                    session_state.clone(),
                ) {
                    Ok(mut worker) => {
                        external_callback(ExternalEvent::Opened(worker.meta.clone()));

                        while let Ok(event) = event_rx.recv() {
                            worker.apply_event(event, &event_rx);
                        }
                    }
                    Err(e) => {
                        error!("Failed to open a playback session for '{}': {:?}", path, e);
                        session_state.store(Arc::new(SessionState::new()));
                        external_callback(ExternalEvent::Error(e));
                    }
                }
            });

        Ok(())
    }

    pub fn apply_event(&self, event: ControlEvent) {
        if let Some(tx) = &self.event_tx
            && let Err(e) = tx.send(PlayerEvent::Control(event))
        {
            error!("Failed to send event: {}", e);
        }
    }

    pub fn active_tracks(&self) -> ActiveTracks {
        self.state.load().tracks
    }

    pub fn playback_mode(&self) -> PlaybackMode {
        self.state.load().mode
    }

    pub fn play(&mut self) {
        self.apply_event(ControlEvent::Play);
    }

    pub fn pause(&mut self) {
        self.apply_event(ControlEvent::Pause);
    }

    pub fn select_track(&mut self, kind: TrackKind, id: usize) {
        let state = self.state.load();

        if !matches!(state.operation, PlaybackOperation::SwitchingTrack { .. }) {
            self.apply_event(ControlEvent::SelectTrack { kind, id });
        }
    }

    pub fn set_audio_master(&mut self, active: bool) {
        self.apply_event(ControlEvent::AudioMaster(active));
    }

    pub fn toggle_playback(&mut self) -> PlaybackMode {
        let state = self.state.load();

        match state.mode {
            PlaybackMode::Playing => {
                self.pause();
                PlaybackMode::Paused
            }
            PlaybackMode::Paused => {
                self.play();
                PlaybackMode::Playing
            }
        }
    }

    pub fn seek_to(&mut self, pos_ms: i64) -> bool {
        let state = self.state.load();

        if matches!(state.operation, PlaybackOperation::Seeking(_)) {
            return false;
        }

        self.apply_event(ControlEvent::Seek(pos_ms));
        true
    }

    pub fn seek_rel(&mut self, offset_ms: i64) {
        let state = self.state.load();

        if !matches!(state.operation, PlaybackOperation::Seeking(_)) {
            self.apply_event(ControlEvent::Seek(self.position_ms() + offset_ms));
        }
    }

    pub fn get_volumes(&self) -> Vec<f32> {
        let volumes = self.audio_info.volume.lock().unwrap();
        volumes.clone()
    }

    pub fn set_volumes(&mut self, volumes: Vec<f32>) {
        self.apply_event(ControlEvent::ChangeVolumes(volumes));
    }

    pub fn modify_volumes(&mut self, f: impl Fn(f32) -> f32) -> f32 {
        let mut volumes = self.audio_info.volume.lock().unwrap();

        for v in volumes.iter_mut() {
            *v = f(*v);
        }

        self.apply_event(ControlEvent::ChangeVolumes(volumes.clone()));
        volumes.first().copied().unwrap_or(0.0)
    }

    pub fn position_ms(&self) -> i64 {
        self.clock.get_ms()
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
}
