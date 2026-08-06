// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Instant;

use gilia_engine::{
    ActiveTracks, ControlEvent, ExternalEvent, PlaybackMode, PlayerMeta, Session, SessionConfig,
    SessionError, TrackKind,
};
#[cfg(feature = "iced")]
use gilia_iced::widget::PlayerWidget;

use crate::player::seek::Seek;

pub mod seek;
pub use seek::{ChapterSeek, SeekMode};

pub struct Player {
    session: Session,
    seek: Seek,
    metadata: Option<PlayerMeta>,
}

impl Player {
    pub fn new<F>(config: SessionConfig, callback: F) -> Self
    where
        F: Fn(ExternalEvent) + Send + Sync + 'static,
    {
        Self {
            session: Session::new(config, callback),
            seek: Seek::default(),
            metadata: None,
        }
    }

    #[cfg(feature = "iced")]
    pub fn view(&self) -> PlayerWidget<'_> {
        PlayerWidget::new(&self.session)
    }

    pub fn metadata(&self) -> Option<&PlayerMeta> {
        self.metadata.as_ref()
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    pub fn playback_mode(&self) -> PlaybackMode {
        self.session.playback_mode()
    }

    pub fn active_tracks(&self) -> ActiveTracks {
        self.session.active_tracks()
    }

    pub fn get_volumes(&self) -> Vec<f32> {
        self.session.get_volumes()
    }

    pub fn position_ms(&self) -> i64 {
        self.session.position_ms()
    }

    pub fn open(&mut self, path: String) -> Result<(), SessionError> {
        self.session.open(path)?;
        self.seek = Seek::default();
        self.metadata = None;

        Ok(())
    }

    pub fn apply_event(&mut self, event: ControlEvent) {
        self.session.apply_event(event);
    }

    pub fn handle_event(&mut self, event: &ExternalEvent) {
        match event {
            ExternalEvent::Opened(metadata) => {
                self.seek = Seek::new(metadata.duration.max(0) / 1_000);
                self.metadata = Some(metadata.clone());
            }
            ExternalEvent::SeekCompleted(position_ms) => {
                self.seek.completed(&mut self.session, *position_ms);
            }
            ExternalEvent::Error(_) => {
                self.seek = Seek::default();
                self.metadata = None;
            }
            _ => {}
        }
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

    pub fn set_volumes(&mut self, volumes: Vec<f32>) {
        self.session.set_volumes(volumes);
    }

    pub fn modify_volumes(&mut self, f: impl Fn(f32) -> f32) -> f32 {
        self.session.modify_volumes(f)
    }
}

impl Player {
    pub fn seek_target_ms(&self) -> Option<i64> {
        self.seek.target_ms()
    }

    pub fn seek_deadline(&self) -> Option<Instant> {
        self.seek.deadline()
    }

    pub fn seek_to(&mut self, position_ms: i64, mode: SeekMode) -> Option<Instant> {
        self.seek.seek_to(&mut self.session, position_ms, mode)
    }

    pub fn seek_rel(&mut self, offset_ms: i64, mode: SeekMode) -> Option<Instant> {
        self.seek.seek_rel(&mut self.session, offset_ms, mode)
    }

    pub fn seek_chapter(&mut self, chapter: ChapterSeek, mode: SeekMode) -> Option<Instant> {
        let Some(metadata) = &self.metadata else {
            return self.seek.deadline();
        };

        self.seek
            .seek_chapter(&mut self.session, &metadata.chapters, chapter, mode)
    }

    pub fn poll_seek(&mut self) -> Option<Instant> {
        if self.seek.is_ready() {
            self.seek.dispatch(&mut self.session);
        }

        self.seek.deadline()
    }
}
