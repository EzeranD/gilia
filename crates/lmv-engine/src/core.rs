use tracing::debug;

use crate::engine::PlayerChannels;

type Pts = i64;

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    Pause,
    Play,
    ChangeVolumes(Vec<f32>),
    Seek(Pts),
    DemuxerSeeked,
    AudioFlushed,
    VideoFlushed,
    AudioBackendFlushed,
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

#[derive(Debug, Clone, Copy)]
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

#[derive(Debug, Clone, Copy)]
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

#[derive(Debug, Clone, Copy)]
pub enum PlaybackMode {
    Paused,
    Playing,
}

#[derive(Debug, Clone, Copy)]
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
            PlayerEvent::Play => {
                self.mode = PlaybackMode::Playing;
                dispatch.ab(AbEffect::Output(true));
            }
            PlayerEvent::Pause => {
                self.mode = PlaybackMode::Paused;
                dispatch.ab(AbEffect::Output(false));
            }
            PlayerEvent::ChangeVolumes(vol) => {
                dispatch.ab(AbEffect::ModifyVolume(vol));
            }
            PlayerEvent::Seek(pts) => {
                self.phase = PlaybackPhase::Seeking(SeekingState::new(pts));
                dispatch.demuxer(DemuxerEffect::SeekDemuxer(pts));
            }
            PlayerEvent::DemuxerSeeked => {
                dispatch.audio(DecoderEffect::Flush);
                dispatch.video(DecoderEffect::Flush);
                dispatch.sub(DecoderEffect::Flush);
            }
            PlayerEvent::AudioFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.audio = TrackSeekState::Flushed;
                    if seeking.is_flushed(&self.streams) {
                        seeking.audio = TrackSeekState::NeedsFlush;
                        seeking.video = TrackSeekState::NeedsFlush;
                        dispatch.flush_frames();
                        dispatch.ab(AbEffect::FlushConsumers);
                    }
                }
            }
            PlayerEvent::VideoFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.video = TrackSeekState::Flushed;
                    if seeking.is_flushed(&self.streams) {
                        seeking.audio = TrackSeekState::NeedsFlush;
                        seeking.video = TrackSeekState::NeedsFlush;
                        dispatch.flush_frames();
                        dispatch.ab(AbEffect::FlushConsumers);
                    }
                }
            }
            PlayerEvent::AudioBackendFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    dispatch.demuxer(DemuxerEffect::ResumeDemuxer);
                    dispatch.audio(DecoderEffect::Sync(seeking.pts));
                    dispatch.video(DecoderEffect::Sync(seeking.pts));
                    dispatch.sub(DecoderEffect::Sync(seeking.pts));
                }
            }
            PlayerEvent::AudioSynced => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.audio = TrackSeekState::Synced;
                    if seeking.is_synced(&self.streams) {
                        self.phase = PlaybackPhase::Normal;
                        if matches!(self.mode, PlaybackMode::Playing) {
                            dispatch.ab(AbEffect::Output(true));
                        }
                    }
                }
            }
            PlayerEvent::VideoSynced => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.video = TrackSeekState::Synced;
                    if seeking.is_synced(&self.streams) {
                        self.phase = PlaybackPhase::Normal;
                        if matches!(self.mode, PlaybackMode::Playing) {
                            dispatch.ab(AbEffect::Output(true));
                        }
                    }
                }
            }
            PlayerEvent::DemuxerEof => {
                self.phase = PlaybackPhase::Draining(DrainingState::new());
                dispatch.audio(DecoderEffect::Drain);
                dispatch.video(DecoderEffect::Drain);
            }
            PlayerEvent::AudioDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.audio = TrackDrainState::DecoderDrained;
                }
                dispatch.ab(AbEffect::DrainOutput);
            }
            PlayerEvent::VideoDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.video = TrackDrainState::DecoderDrained;
                }
                dispatch.ab(AbEffect::DrainOutput);
            }
            PlayerEvent::FramesDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.video = TrackDrainState::OutputDrained;
                    if draining.is_drained(&self.streams) {
                        dispatch.ab(AbEffect::Output(false));
                        self.phase = PlaybackPhase::Eof;
                    }
                }
            }
            PlayerEvent::SamplesDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.audio = TrackDrainState::OutputDrained;
                    if draining.is_drained(&self.streams) {
                        dispatch.ab(AbEffect::Output(false));
                        self.phase = PlaybackPhase::Eof;
                    }
                }
            }
        }
    }
}
