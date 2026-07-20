use std::{sync::atomic::Ordering, thread::JoinHandle};

use crossbeam_channel::Sender;
use ffmpeg_next::{format::input, media::Type};
use tracing::debug;

use crate::{
    ExternalEvent, SharedPlayerState,
    audio::spawn_audio_stream,
    demuxer::Demuxer,
    engine::{EngineError, ExternalCallback, PlayerChannels, get_engine_start, get_stream},
    subtitle::spawn_sub_stream,
    video::spawn_video_stream,
};

type Pts = i64;

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    Control(ControlEvent),
    Internal(InternalEvent),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlEvent {
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

pub struct ActiveStreams {
    pub subs: bool,
    pub audio: bool,
    pub video: bool,
}

pub struct PlayerThreads {
    pub demuxer: Option<JoinHandle<()>>,
    pub audio_decoder: Option<JoinHandle<()>>,
    pub audio_backend: Option<JoinHandle<()>>,
    pub video_decoder: Option<JoinHandle<()>>,
    pub video_output: Option<JoinHandle<()>>,
    pub sub_decoder: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeekingState {
    pub pts: Pts,
    pub audio: TrackSeekState,
    pub video: TrackSeekState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrainingState {
    pub audio: TrackDrainState,
    pub video: TrackDrainState,
}

impl PlayerThreads {
    pub fn new() -> Self {
        Self {
            demuxer: None,
            audio_decoder: None,
            audio_backend: None,
            video_decoder: None,
            video_output: None,
            sub_decoder: None,
        }
    }
}

impl Default for PlayerThreads {
    fn default() -> Self {
        Self::new()
    }
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

pub struct PlayerSession {
    mode: PlaybackMode,
    phase: PlaybackPhase,
    shared: SharedPlayerState,
    external_callback: ExternalCallback,
    event_tx: Sender<PlayerEvent>,
    streams: ActiveStreams,
    channels: PlayerChannels,
    threads: PlayerThreads,
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

impl PlayerSession {
    pub fn open(
        path: &str,
        shared: SharedPlayerState,
        external_callback: &ExternalCallback,
        event_tx: Sender<PlayerEvent>,
    ) -> Result<Self, EngineError> {
        let (streams, channels, threads) = open(path, &shared, external_callback, &event_tx)?;
        Ok(Self {
            mode: PlaybackMode::Playing,
            phase: PlaybackPhase::Normal,
            shared,
            external_callback: external_callback.clone(),
            event_tx,
            streams,
            channels,
            threads,
        })
    }
    pub fn snapshot(&mut self) -> PlayerSnapshot {
        PlayerSnapshot {
            mode: self.mode,
            phase: self.phase,
        }
    }

    pub fn apply_event(&mut self, event: PlayerEvent) {
        debug!("Event received: {event:?}");
        match event {
            PlayerEvent::Control(event) => {
                self.apply_control_event(event);
            }
            PlayerEvent::Internal(event) => {
                self.apply_internal_event(event);
            }
        }
    }

    fn apply_control_event(&mut self, event: ControlEvent) {
        match event {
            ControlEvent::Play => {
                self.mode = PlaybackMode::Playing;
                self.channels.ab(AbEffect::Output(true));
                self.channels.vp(VoEffect::Output(true));
            }
            ControlEvent::Pause => {
                self.mode = PlaybackMode::Paused;
                self.channels.ab(AbEffect::Output(false));
                self.channels.vp(VoEffect::Output(false));
            }
            ControlEvent::Seek(pts) => {
                self.phase = PlaybackPhase::Seeking(SeekingState::new(pts));
                self.channels.demuxer(DemuxerEffect::SeekDemuxer(pts));
                self.channels.ab(AbEffect::Output(false));
                self.channels.vp(VoEffect::Output(false));
                if let Some(clock) = &self.channels.audio_clock {
                    let now = get_engine_start().elapsed().as_nanos() as i64;
                    clock.update(pts * 1_000_000, now);
                }
            }
            ControlEvent::ChangeVolumes(vol) => {
                self.channels.ab(AbEffect::ModifyVolume(vol));
            }
        }
    }

    fn apply_internal_event(&mut self, event: InternalEvent) {
        match event {
            InternalEvent::DemuxerSeeked => {
                self.channels.audio(DecoderEffect::Flush);
                self.channels.video(DecoderEffect::Flush);
                self.channels.sub(DecoderEffect::Flush);
            }
            InternalEvent::AudioFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.audio = TrackSeekState::Flushed;
                    if seeking.is_flushed(&self.streams) {
                        seeking.audio = TrackSeekState::NeedsFlush;
                        seeking.video = TrackSeekState::NeedsFlush;
                        self.channels.vp(VoEffect::FlushConsumers);
                        self.channels.ab(AbEffect::FlushConsumers);
                    }
                }
            }
            InternalEvent::VideoFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.video = TrackSeekState::Flushed;
                    if seeking.is_flushed(&self.streams) {
                        seeking.audio = TrackSeekState::NeedsFlush;
                        seeking.video = TrackSeekState::NeedsFlush;
                        self.channels.vp(VoEffect::FlushConsumers);
                        self.channels.ab(AbEffect::FlushConsumers);
                    }
                }
            }
            InternalEvent::AudioBackendFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.audio = TrackSeekState::Flushed;
                    if seeking.is_flushed(&self.streams) {
                        self.channels.demuxer(DemuxerEffect::ResumeDemuxer);
                        self.channels.audio(DecoderEffect::Sync(seeking.pts));
                        self.channels.video(DecoderEffect::Sync(seeking.pts));
                        self.channels.sub(DecoderEffect::Sync(seeking.pts));
                    }
                }
            }
            InternalEvent::VideoOutputFlushed => {
                if let PlaybackPhase::Seeking(seeking) = &mut self.phase {
                    seeking.video = TrackSeekState::Flushed;
                    if seeking.is_flushed(&self.streams) {
                        self.channels.demuxer(DemuxerEffect::ResumeDemuxer);
                        self.channels.audio(DecoderEffect::Sync(seeking.pts));
                        self.channels.video(DecoderEffect::Sync(seeking.pts));
                        self.channels.sub(DecoderEffect::Sync(seeking.pts));
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
                                self.channels.ab(AbEffect::Output(true));
                                self.channels.vp(VoEffect::Present);
                                self.channels.vp(VoEffect::Output(true));
                            }
                            PlaybackMode::Paused => self.channels.vp(VoEffect::Present),
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
                                self.channels.ab(AbEffect::Output(true));
                                self.channels.vp(VoEffect::Present);
                                self.channels.vp(VoEffect::Output(true));
                            }
                            PlaybackMode::Paused => self.channels.vp(VoEffect::Present),
                            PlaybackMode::Stopped => {}
                        }
                    }
                }
            }
            InternalEvent::DemuxerEof => {
                self.phase = PlaybackPhase::Draining(DrainingState::new());
                self.channels.audio(DecoderEffect::Drain);
                self.channels.video(DecoderEffect::Drain);
            }
            InternalEvent::AudioDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.audio = TrackDrainState::DecoderDrained;
                }
                self.channels.ab(AbEffect::DrainOutput);
                self.channels.vp(VoEffect::DrainOutput);
            }
            InternalEvent::VideoDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.video = TrackDrainState::DecoderDrained;
                }
                self.channels.ab(AbEffect::DrainOutput);
                self.channels.vp(VoEffect::DrainOutput);
            }
            InternalEvent::FramesDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.video = TrackDrainState::OutputDrained;
                    if draining.is_drained(&self.streams) {
                        self.mode = PlaybackMode::Stopped;
                        self.phase = PlaybackPhase::Eof;
                        if let Some(cb) = &self.channels.external_callback {
                            cb(ExternalEvent::Eof);
                        }
                    }
                }
            }
            InternalEvent::SamplesDrained => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    self.channels.ab(AbEffect::Output(false));
                    draining.audio = TrackDrainState::OutputDrained;
                    if draining.is_drained(&self.streams) {
                        self.mode = PlaybackMode::Stopped;
                        self.phase = PlaybackPhase::Eof;
                        if let Some(cb) = &self.channels.external_callback {
                            cb(ExternalEvent::Eof);
                        }
                    }
                }
            }
        }
    }
}

pub fn open(
    path: &str,
    shared: &SharedPlayerState,
    external_callback: &ExternalCallback,
    event_tx: &Sender<PlayerEvent>,
) -> Result<(ActiveStreams, PlayerChannels, PlayerThreads), EngineError> {
    let ictx = input(&path)?;

    let mut channels = PlayerChannels::new();
    let mut threads = PlayerThreads::new();
    channels.external_callback = Some(external_callback.clone());

    let audio_stream = get_stream(&ictx, Type::Audio);
    let video_stream = get_stream(&ictx, Type::Video);
    let sub_stream = get_stream(&ictx, Type::Subtitle);

    let mut audio_idx = None;
    let mut video_idx = None;
    let mut sub_idx = None;

    if let Some(audio_stream) = audio_stream {
        audio_idx = Some(audio_stream.1);
        let (dec_handle, ab_handle, dec_effect_tx, pw_tx, packet_tx) = spawn_audio_stream(
            &audio_stream,
            event_tx,
            external_callback,
            &shared.audio_info.volume,
            shared.audio_info.audio_clock.clone(),
            path,
        );

        channels.audio_tx = Some(dec_effect_tx);
        channels.pw_tx = Some(pw_tx);
        channels.audio_packet_tx = Some(packet_tx);
        channels.audio_clock = Some(shared.audio_info.audio_clock.clone());
        shared
            .audio_info
            .audio_clock
            .active
            .store(true, Ordering::Relaxed);
        threads.audio_decoder = Some(dec_handle);
        threads.audio_backend = Some(ab_handle);
    }

    if let Some(video_stream) = video_stream {
        video_idx = Some(video_stream.1);
        let (dec_handle, output_handle, dec_tx, vo_tx, packet_tx) = spawn_video_stream(
            &video_stream,
            event_tx,
            Some(external_callback),
            &shared.config,
            &shared.video_output.frame,
            &shared.video_output.current_pts,
            &shared.audio_info.audio_clock,
        );
        channels.video_tx = Some(dec_tx);
        channels.video_output_tx = Some(vo_tx);
        channels.video_packet_tx = Some(packet_tx);
        threads.video_decoder = Some(dec_handle);
        threads.video_output = Some(output_handle);
    }

    if let Some(sub_stream) = sub_stream {
        sub_idx = Some(sub_stream.1);
        let (dec_handle, sub_tx, packet_tx) = spawn_sub_stream(
            &ictx,
            event_tx,
            sub_stream,
            shared.sub_output.track.clone(),
            shared.sub_output.renderer.clone(),
        );
        channels.sub_tx = Some(sub_tx);
        channels.sub_packet_tx = Some(packet_tx);
        threads.sub_decoder = Some(dec_handle);
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
    let demuxer_handle = std::thread::Builder::new()
        .name("demuxer".into())
        .spawn(move || {
            demuxer.read_packets();
        })
        .unwrap();
    threads.demuxer = Some(demuxer_handle);

    Ok((
        ActiveStreams {
            subs: sub_idx.is_some(),
            audio: audio_idx.is_some(),
            video: video_idx.is_some(),
        },
        channels,
        threads,
    ))
}
