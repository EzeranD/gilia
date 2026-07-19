use tracing::debug;

use crate::{ExternalEvent, engine::PlayerChannels};

type Pts = i64;

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    Control(ControlEvent),
    Internal(InternalEvent),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlEvent {
    Open(String),
    Play,
    Pause,
    Seek(Pts),
    ChangeVolumes(Vec<f32>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InternalEvent {
    DemuxerSeeked,
    AudioFlushed,
    VideoFlushed,
    AudioBackendFlushed,
    VideoOutputFlushed,
    AudioSynced,
    VideoSynced,
    DemuxerEof,
    AudioDrained,
    VideoDrained,
    FramesDrained,
    SamplesDrained,
}

#[derive(Debug, Clone, Copy)]
pub enum DemuxerEffect {
    SeekDemuxer(Pts),
    ResumeDemuxer,
}

#[derive(Debug, Clone, Copy)]
pub enum DecoderEffect {
    Flush,
    Sync(Pts),
    Drain,
}

#[derive(Debug)]
pub enum AbEffect {
    Output(bool),
    FlushConsumers,
    ModifyVolume(Vec<f32>),
    DrainOutput,
}

#[derive(Debug, Clone, Copy)]
pub enum VoEffect {
    Output(bool),
    Present,
    FlushConsumers,
    DrainOutput,
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
    DecoderDrained,
    OutputDrained,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeekingState {
    pub pts: Pts,
    pub audio: TrackSeekState,
    pub video: TrackSeekState,
}

impl SeekingState {
    pub fn new(pts: Pts) -> Self {
        Self {
            pts,
            audio: TrackSeekState::NeedsFlush,
            video: TrackSeekState::NeedsFlush,
        }
    }

    pub fn is_flushed(&self, streams: &ActiveStreams) -> bool {
        let audio_done = !streams.audio
            || matches!(self.audio, TrackSeekState::Flushed | TrackSeekState::Synced);
        let video_done = !streams.video
            || matches!(self.video, TrackSeekState::Flushed | TrackSeekState::Synced);
        audio_done && video_done
    }

    pub fn is_synced(&self, streams: &ActiveStreams) -> bool {
        let audio_done = !streams.audio || matches!(self.audio, TrackSeekState::Synced);
        let video_done = !streams.video || matches!(self.video, TrackSeekState::Synced);
        audio_done && video_done
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrainingState {
    pub audio: TrackDrainState,
    pub video: TrackDrainState,
}

impl DrainingState {
    pub fn new() -> Self {
        Self {
            audio: TrackDrainState::Active,
            video: TrackDrainState::Active,
        }
    }

    pub fn is_drained(&self, streams: &ActiveStreams) -> bool {
        let audio_done = !streams.audio || matches!(self.audio, TrackDrainState::OutputDrained);
        let video_done = !streams.video || matches!(self.video, TrackDrainState::OutputDrained);
        audio_done && video_done
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PlayerSnapshot {
    pub mode: PlaybackMode,
    pub phase: PlaybackPhase,
}

pub struct PlayerCore {
    mode: PlaybackMode,
    phase: PlaybackPhase,
    streams: ActiveStreams,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackMode {
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackPhase {
    Normal,
    Seeking(SeekingState),
    Draining(DrainingState),
    Eof,
}

pub struct ActiveStreams {
    pub subs: bool,
    pub audio: bool,
    pub video: bool,
}

impl PlayerCore {
    #[must_use]
    pub fn new(streams: ActiveStreams) -> Self {
        Self {
            mode: PlaybackMode::Playing,
            phase: PlaybackPhase::Normal,
            streams,
        }
    }
    pub fn snapshot(&mut self) -> PlayerSnapshot {
        PlayerSnapshot {
            mode: self.mode,
            phase: self.phase,
        }
    }

    pub fn apply_event(&mut self, event: PlayerEvent, dispatch: &PlayerChannels) {
        debug!("Event received: {event:?}");
        match event {
            PlayerEvent::Control(event) => {
                self.apply_control_event(event, dispatch);
            }
            PlayerEvent::Internal(event) => {
                self.apply_internal_event(event, dispatch);
            }
        }
    }
    fn apply_control_event(&mut self, event: ControlEvent, dispatch: &PlayerChannels) {
        match event {
            ControlEvent::Open(path) => {}
            ControlEvent::Play => {
                self.mode = PlaybackMode::Playing;
                dispatch.ab(AbEffect::Output(true));
                dispatch.vp(VoEffect::Output(true));
            }
            ControlEvent::Pause => {
                self.mode = PlaybackMode::Paused;
                dispatch.ab(AbEffect::Output(false));
                dispatch.vp(VoEffect::Output(false));
            }
            ControlEvent::Seek(pts) => {
                self.phase = PlaybackPhase::Seeking(SeekingState::new(pts));
                dispatch.demuxer(DemuxerEffect::SeekDemuxer(pts));
                dispatch.ab(AbEffect::Output(false));
                dispatch.vp(VoEffect::Output(false));
                if let Some(clock) = &dispatch.audio_clock {
                    let now = crate::engine::get_engine_start().elapsed().as_nanos() as i64;
                    clock.update(pts * 1_000_000, now);
                }
            }
            ControlEvent::ChangeVolumes(vol) => {
                dispatch.ab(AbEffect::ModifyVolume(vol));
            }
        }
    }

    fn apply_internal_event(&mut self, event: InternalEvent, dispatch: &PlayerChannels) {
        match event {
            InternalEvent::DemuxerSeeked => {
                dispatch.audio(DecoderEffect::Flush);
                dispatch.video(DecoderEffect::Flush);
                dispatch.sub(DecoderEffect::Flush);
            }
            InternalEvent::AudioFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.audio = TrackSeekState::Flushed;
                    if seeking.is_flushed(&self.streams) {
                        seeking.audio = TrackSeekState::NeedsFlush;
                        seeking.video = TrackSeekState::NeedsFlush;
                        dispatch.vp(VoEffect::FlushConsumers);
                        dispatch.ab(AbEffect::FlushConsumers);
                    }
                }
            }
            InternalEvent::VideoFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.video = TrackSeekState::Flushed;
                    if seeking.is_flushed(&self.streams) {
                        seeking.audio = TrackSeekState::NeedsFlush;
                        seeking.video = TrackSeekState::NeedsFlush;
                        dispatch.vp(VoEffect::FlushConsumers);
                        dispatch.ab(AbEffect::FlushConsumers);
                    }
                }
            }
            InternalEvent::AudioBackendFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.audio = TrackSeekState::Flushed;
                    if seeking.is_flushed(&self.streams) {
                        dispatch.demuxer(DemuxerEffect::ResumeDemuxer);
                        dispatch.audio(DecoderEffect::Sync(seeking.pts));
                        dispatch.video(DecoderEffect::Sync(seeking.pts));
                        dispatch.sub(DecoderEffect::Sync(seeking.pts));
                    }
                }
            }
            InternalEvent::VideoOutputFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.video = TrackSeekState::Flushed;
                    if seeking.is_flushed(&self.streams) {
                        dispatch.demuxer(DemuxerEffect::ResumeDemuxer);
                        dispatch.audio(DecoderEffect::Sync(seeking.pts));
                        dispatch.video(DecoderEffect::Sync(seeking.pts));
                        dispatch.sub(DecoderEffect::Sync(seeking.pts));
                    }
                }
            }
            InternalEvent::AudioSynced => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.audio = TrackSeekState::Synced;
                    if seeking.is_synced(&self.streams) {
                        self.phase = PlaybackPhase::Normal;
                        match self.mode {
                            PlaybackMode::Playing => {
                                dispatch.ab(AbEffect::Output(true));
                                dispatch.vp(VoEffect::Present);
                                dispatch.vp(VoEffect::Output(true));
                            }
                            PlaybackMode::Paused => dispatch.vp(VoEffect::Present),
                            PlaybackMode::Stopped => {}
                        }
                    }
                }
            }
            InternalEvent::VideoSynced => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.video = TrackSeekState::Synced;
                    if seeking.is_synced(&self.streams) {
                        self.phase = PlaybackPhase::Normal;
                        match self.mode {
                            PlaybackMode::Playing => {
                                dispatch.ab(AbEffect::Output(true));
                                dispatch.vp(VoEffect::Present);
                                dispatch.vp(VoEffect::Output(true));
                            }
                            PlaybackMode::Paused => dispatch.vp(VoEffect::Present),
                            PlaybackMode::Stopped => {}
                        }
                    }
                }
            }
            InternalEvent::DemuxerEof => {
                self.phase = PlaybackPhase::Draining(DrainingState::new());
                dispatch.audio(DecoderEffect::Drain);
                dispatch.video(DecoderEffect::Drain);
            }
            InternalEvent::AudioDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.audio = TrackDrainState::DecoderDrained;
                }
                dispatch.ab(AbEffect::DrainOutput);
                dispatch.vp(VoEffect::DrainOutput);
            }
            InternalEvent::VideoDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.video = TrackDrainState::DecoderDrained;
                }
                dispatch.ab(AbEffect::DrainOutput);
                dispatch.vp(VoEffect::DrainOutput);
            }
            InternalEvent::FramesDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.video = TrackDrainState::OutputDrained;
                    if draining.is_drained(&self.streams) {
                        self.mode = PlaybackMode::Stopped;
                        self.phase = PlaybackPhase::Eof;
                        if let Some(cb) = &dispatch.external_callback {
                            cb(ExternalEvent::Eof);
                        }
                    }
                }
            }
            InternalEvent::SamplesDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    dispatch.ab(AbEffect::Output(false));
                    draining.audio = TrackDrainState::OutputDrained;
                    if draining.is_drained(&self.streams) {
                        self.mode = PlaybackMode::Stopped;
                        self.phase = PlaybackPhase::Eof;
                        if let Some(cb) = &dispatch.external_callback {
                            cb(ExternalEvent::Eof);
                        }
                    }
                }
            }
        }
    }
}
