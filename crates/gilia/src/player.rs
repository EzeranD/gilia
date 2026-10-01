// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gilia_engine::{
    ActiveTracks, ControlEvent, ExternalEvent, PlaybackMode, Session, SessionConfig, TrackKind,
};
#[cfg(feature = "iced")]
use gilia_iced::widget::PlayerWidget;

pub struct Player {
    session: Session,
}

impl Player {
    pub fn new<F>(path: String, config: SessionConfig, callback: F) -> Self
    where
        F: Fn(ExternalEvent) + Send + Sync + 'static,
    {
        let mut session = Session::new(config, callback);
        let _ = session.open(path);

        Self { session }
    }

    #[cfg(feature = "iced")]
    pub fn view(&self) -> PlayerWidget<'_> {
        PlayerWidget::new(&self.session)
    }

    pub fn apply_event(&mut self, event: ControlEvent) {
        self.session.apply_event(event);
    }

    pub fn active_tracks(&self) -> ActiveTracks {
        self.session.active_tracks()
    }

    pub fn play(&mut self) {
        self.session.play();
    }

    pub fn pause(&mut self) {
        self.session.pause();
    }

    pub fn select_track(&mut self, kind: TrackKind, id: usize) {
        self.session.select_track(kind, id);
    }

    pub fn set_audio_master(&mut self, active: bool) {
        self.session.set_audio_master(active);
    }

    pub fn toggle_playback(&mut self) -> PlaybackMode {
        self.session.toggle_playback()
    }

    pub fn seek_to(&mut self, pos_ms: i64) {
        self.session.seek_to(pos_ms);
    }

    pub fn seek_rel(&mut self, offset_ms: i64) {
        self.session.seek_rel(offset_ms);
    }

    pub fn get_volumes(&self) -> Vec<f32> {
        self.session.get_volumes()
    }

    pub fn set_volumes(&mut self, volumes: Vec<f32>) {
        self.session.set_volumes(volumes);
    }

    pub fn modify_volumes(&mut self, f: impl Fn(f32) -> f32) -> f32 {
        self.session.modify_volumes(f)
    }

    pub fn position_ms(&self) -> i64 {
        self.session.position_ms()
    }
}
