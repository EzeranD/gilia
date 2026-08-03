// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gilia_engine::{
    ActiveTracks,
    ControlEvent::{self, AudioMaster, ChangeVolumes, Pause, Play, Seek, SelectTrack},
    EngineConfig, ExternalEvent, PlaybackMode, PlaybackOperation, PlayerEngine,
    PlayerEvent::Control,
    TrackKind,
};
#[cfg(feature = "iced")]
use gilia_iced::widget::PlayerWidget;

pub struct Player {
    engine: PlayerEngine,
}

impl Player {
    pub fn new<F>(path: String, config: EngineConfig, callback: F) -> Self
    where
        F: Fn(ExternalEvent) + Send + Sync + 'static,
    {
        let mut engine = PlayerEngine::new(config, callback);
        let _ = engine.open(path);
        Self { engine }
    }

    #[cfg(feature = "iced")]
    pub fn view(&self) -> PlayerWidget<'_> {
        PlayerWidget::new(&self.engine)
    }

    pub fn apply_event(&mut self, event: ControlEvent) {
        self.engine.apply_event(Control(event));
    }

    pub fn active_tracks(&self) -> ActiveTracks {
        self.engine.active_tracks()
    }

    pub fn play(&mut self) {
        self.engine.apply_event(Control(Play));
    }

    pub fn pause(&mut self) {
        self.engine.apply_event(Control(Pause));
    }

    pub fn select_track(&mut self, kind: TrackKind, id: usize) {
        let state = self.engine.state.load();
        if !matches!(state.operation, PlaybackOperation::SwitchingTrack { .. }) {
            self.engine.apply_event(Control(SelectTrack { kind, id }));
        }
    }

    pub fn set_audio_master(&mut self, active: bool) {
        self.engine.apply_event(Control(AudioMaster(active)));
    }

    pub fn toggle_playback(&mut self) -> PlaybackMode {
        let state = self.engine.state.load();
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

    pub fn seek_to(&mut self, pos_ms: i64) {
        let state = self.engine.state.load();
        if !matches!(state.operation, PlaybackOperation::Seeking(_)) {
            self.engine.apply_event(Control(Seek(pos_ms)));
        }
    }

    pub fn seek_rel(&mut self, offset_ms: i64) {
        let state = self.engine.state.load();
        if !matches!(state.operation, PlaybackOperation::Seeking(_)) {
            self.engine
                .apply_event(Control(Seek(self.position_ms() + offset_ms)));
        }
    }

    #[allow(dead_code)]
    pub fn get_volumes(&self) -> Vec<f32> {
        let volumes = self.engine.audio_info.volume.lock().unwrap();
        volumes.clone()
    }

    #[allow(dead_code)]
    pub fn set_volumes(&mut self, volumes: Vec<f32>) {
        self.engine.apply_event(Control(ChangeVolumes(volumes)));
    }

    pub fn modify_volumes(&mut self, f: impl Fn(f32) -> f32) -> f32 {
        let mut volumes = self.engine.audio_info.volume.lock().unwrap();
        for v in volumes.iter_mut() {
            *v = f(*v);
        }
        self.engine
            .apply_event(Control(ChangeVolumes(volumes.clone())));
        volumes.first().copied().unwrap_or(0.0)
    }

    pub fn position_ms(&self) -> i64 {
        self.engine.position_ms()
    }
}
