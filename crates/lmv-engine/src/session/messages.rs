use crossbeam_channel::Sender;
use ffmpeg_next::{Rational, codec::Parameters};

use super::{TrackId, TrackKind, Worker};
use crate::{engine::ExternalCallback, session::Pts, utils::WakingSender};

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    Control(ControlEvent),
    Internal(InternalEvent),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlEvent {
    Play,
    Pause,
    ChangeVolumes(Vec<f32>),
    AudioMaster(bool),
    Seek(Pts),
    SelectTrack { id: TrackId, kind: TrackKind },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InternalEvent {
    DemuxerEof,
    Drained(Worker),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReinitDetails {
    None,
    Audio { rate: u32 },
}

#[derive(Debug, Clone)]
pub enum DemuxerEffect {
    ResumeDemuxer,
    SeekDemuxer(Sender<()>, Pts),
    SwitchStream {
        seek_tx: Sender<()>,
        kind: TrackKind,
        id: TrackId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Target(super::Pts),
    FollowClock,
}

#[derive(Clone)]
pub enum DecoderEffect {
    Flush(Sender<Worker>),
    Sync(Sender<Worker>, SyncMode),
    Drain,
    Reinit(Sender<(Worker, ReinitDetails)>, Parameters, Rational),
}

#[derive(Debug)]
pub enum AbEffect {
    Output(bool),
    FlushConsumers(Sender<Worker>),
    Reinit(u32),
    ModifyVolume(Vec<f32>),
    DrainOutput,
}

#[derive(Debug, Clone)]
pub enum VoEffect {
    Output(bool),
    Present,
    FlushConsumers(Sender<Worker>),
    DrainOutput,
}

pub(crate) struct SessionChannels {
    pub(crate) demuxer: Sender<DemuxerEffect>,
    pub(crate) audio: Option<AudioChannels>,
    pub(crate) video: Option<VideoChannels>,
    pub(crate) sub: Option<SubtitleChannels>,
    pub(crate) external_callback: ExternalCallback,
}

pub(crate) struct AudioChannels {
    pub(crate) decoder: WakingSender<DecoderEffect>,
    pub(crate) output: pipewire::channel::Sender<AbEffect>,
}

pub(crate) struct VideoChannels {
    pub(crate) decoder: Sender<DecoderEffect>,
    pub(crate) output: Sender<VoEffect>,
}

pub(crate) struct SubtitleChannels {
    pub(crate) decoder: Sender<DecoderEffect>,
}

impl std::fmt::Debug for DecoderEffect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecoderEffect::Flush(sender) => write!(f, "Flush({sender:?})"),
            DecoderEffect::Sync(sender, mode) => write!(f, "Sync({sender:?}, {mode:?})"),
            DecoderEffect::Drain => write!(f, "Drain"),
            DecoderEffect::Reinit(_, _, _) => write!(f, "Reinit"),
        }
    }
}

impl SessionChannels {
    pub(crate) fn new(demuxer: Sender<DemuxerEffect>, external_callback: ExternalCallback) -> Self {
        Self {
            demuxer,
            audio: None,
            video: None,
            sub: None,
            external_callback,
        }
    }

    pub(crate) fn ab(&self, effect: AbEffect) {
        if let Some(audio) = &self.audio {
            let _ = audio.output.send(effect);
        }
    }

    pub(crate) fn demuxer(&self, effect: DemuxerEffect) {
        let _ = self.demuxer.send(effect);
    }

    pub(crate) fn audio(&self, effect: DecoderEffect) {
        if let Some(audio) = &self.audio {
            let _ = audio.decoder.send(effect);
        }
    }

    pub(crate) fn video(&self, effect: DecoderEffect) {
        if let Some(video) = &self.video {
            let _ = video.decoder.send(effect);
        }
    }

    pub(crate) fn sub(&self, effect: DecoderEffect) {
        if let Some(sub) = &self.sub {
            let _ = sub.decoder.send(effect);
        }
    }

    pub(crate) fn vo(&self, effect: VoEffect) {
        if let Some(video) = &self.video {
            let _ = video.output.send(effect);
        }
    }
}
