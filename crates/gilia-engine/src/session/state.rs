// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::TrackId;

pub type Pts = i64;

#[derive(Debug, Clone, Copy)]
pub struct SessionState {
    pub mode: PlaybackMode,
    pub operation: PlaybackOperation,
    pub phase: PlaybackPhase,
    pub tracks: ActiveTracks,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackMode {
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackOperation {
    None,
    Seeking(SeekingState),
    SwitchingTrack { kind: super::TrackKind, id: TrackId },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackPhase {
    Active,
    InputExhausted,
    Draining(DrainingState),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSeekState {
    NeedsFlush,
    Flushed,
    Synced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackDrainState {
    Active,
    Drained,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveTracks {
    pub audio: Option<TrackId>,
    pub video: Option<TrackId>,
    pub subs: Option<TrackId>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeekingState {
    pub pts: Pts,
    pub audio_dec: TrackSeekState,
    pub video_dec: TrackSeekState,
    pub sub_dec: TrackSeekState,
    pub audio_out: TrackSeekState,
    pub video_out: TrackSeekState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrainingState {
    pub audio_dec: TrackDrainState,
    pub video_dec: TrackDrainState,
    pub audio_out: TrackDrainState,
    pub video_out: TrackDrainState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Worker {
    AudioDecoder,
    VideoDecoder,
    SubDecoder,
    AudioOutput,
    VideoOutput,
}

impl SessionState {
    pub fn new(tracks: ActiveTracks) -> Self {
        Self {
            mode: PlaybackMode::Playing,
            operation: PlaybackOperation::None,
            phase: PlaybackPhase::Active,
            tracks,
        }
    }
    pub fn is_busy(&self) -> bool {
        self.operation != PlaybackOperation::None
    }
}

impl SeekingState {
    pub fn new(pts: Pts) -> Self {
        Self {
            pts,
            audio_dec: TrackSeekState::NeedsFlush,
            video_dec: TrackSeekState::NeedsFlush,
            sub_dec: TrackSeekState::NeedsFlush,
            audio_out: TrackSeekState::NeedsFlush,
            video_out: TrackSeekState::NeedsFlush,
        }
    }

    pub fn set_state(&mut self, worker: Worker, state: TrackSeekState) {
        match worker {
            Worker::AudioDecoder => self.audio_dec = state,
            Worker::VideoDecoder => self.video_dec = state,
            Worker::AudioOutput => self.audio_out = state,
            Worker::VideoOutput => self.video_out = state,
            Worker::SubDecoder => self.sub_dec = state,
        }
    }

    pub fn dec_flushed(&self, streams: &ActiveTracks) -> bool {
        let audio_done =
            streams.audio.is_none() || matches!(self.audio_dec, TrackSeekState::Flushed);
        let video_done =
            streams.video.is_none() || matches!(self.video_dec, TrackSeekState::Flushed);
        let sub_done = streams.subs.is_none() || matches!(self.sub_dec, TrackSeekState::Flushed);
        audio_done && video_done && sub_done
    }

    pub fn out_flushed(&self, streams: &ActiveTracks) -> bool {
        let audio_done =
            streams.audio.is_none() || matches!(self.audio_out, TrackSeekState::Flushed);
        let video_done =
            streams.video.is_none() || matches!(self.video_out, TrackSeekState::Flushed);
        audio_done && video_done
    }

    pub fn dec_synced(&self, streams: &ActiveTracks) -> bool {
        let audio_done =
            streams.audio.is_none() || matches!(self.audio_dec, TrackSeekState::Synced);
        let video_done =
            streams.video.is_none() || matches!(self.video_dec, TrackSeekState::Synced);
        let sub_done = streams.subs.is_none() || matches!(self.sub_dec, TrackSeekState::Synced);
        audio_done && video_done && sub_done
    }
}

impl DrainingState {
    pub fn new() -> Self {
        Self {
            audio_dec: TrackDrainState::Active,
            video_dec: TrackDrainState::Active,
            audio_out: TrackDrainState::Active,
            video_out: TrackDrainState::Active,
        }
    }

    pub fn set_state(&mut self, worker: Worker, state: TrackDrainState) {
        match worker {
            Worker::AudioDecoder => self.audio_dec = state,
            Worker::VideoDecoder => self.video_dec = state,
            Worker::AudioOutput => self.audio_out = state,
            Worker::VideoOutput => self.video_out = state,
            Worker::SubDecoder => todo!(),
        }
    }

    pub fn dec_drained(self, streams: &ActiveTracks) -> bool {
        let audio_done =
            streams.audio.is_none() || matches!(self.audio_dec, TrackDrainState::Drained);
        let video_done =
            streams.video.is_none() || matches!(self.video_dec, TrackDrainState::Drained);
        audio_done && video_done
    }

    pub fn out_drained(self, streams: &ActiveTracks) -> bool {
        let audio_done =
            streams.audio.is_none() || matches!(self.audio_out, TrackDrainState::Drained);
        let video_done =
            streams.video.is_none() || matches!(self.video_out, TrackDrainState::Drained);
        audio_done && video_done
    }
}
