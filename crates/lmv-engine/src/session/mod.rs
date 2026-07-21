use std::{
    sync::{Arc, atomic::Ordering},
    thread::JoinHandle,
};

use crossbeam_channel::Sender;
use ffmpeg_next::{Rational, codec::Parameters, format::input, media::Type};
use tracing::debug;

mod metadata;
mod state;

pub use metadata::{PlayerMeta, TrackId, TrackKind, TrackMeta};
pub use state::{
    ActiveTracks, DrainingState, PlaybackMode, PlaybackOperation, PlaybackPhase, PlayerSnapshot,
    SeekingState, TrackDrainState, TrackSeekState, Worker,
};

use crate::{
    ExternalEvent, SharedPlayerState,
    audio::spawn_audio_stream,
    demuxer::Demuxer,
    engine::{EngineError, ExternalCallback, PlayerChannels, get_engine_start},
    subtitle::spawn_sub_stream,
    utils::{MemoryBudget, ThreadWaker},
    video::spawn_video_stream,
};

pub type Pts = state::Pts;

pub struct PlayerSession {
    pub mode: PlaybackMode,
    pub operation: PlaybackOperation,
    pub phase: PlaybackPhase,
    pub shared: SharedPlayerState,
    pub external_callback: ExternalCallback,
    pub event_tx: Sender<PlayerEvent>,
    pub streams: ActiveTracks,
    pub channels: PlayerChannels,
    pub threads: PlayerThreads,
    pub meta: PlayerMeta,
}

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
    AudioMaster(bool),
    SelectTrack { kind: TrackKind, id: TrackId },
    ChangeVolumes(Vec<f32>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InternalEvent {
    DemuxerSeeked,
    Flushed(Worker),
    Synced(Worker),
    Reinitialized {
        worker: Worker,
        details: ReinitDetails,
    },
    DemuxerEof,
    Drained(Worker),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReinitDetails {
    None,
    Audio { rate: u32 },
}

#[derive(Debug, Clone, Copy)]
pub enum DemuxerEffect {
    SeekDemuxer(Pts),
    ResumeDemuxer,
    SwitchStream { kind: TrackKind, id: TrackId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Target(Pts),
    FollowClock,
}

#[derive(Clone)]
pub enum DecoderEffect {
    Flush,
    Sync(SyncMode),
    Drain,
    Reinit(Parameters, Rational),
}

#[derive(Debug)]
pub enum AbEffect {
    Output(bool),
    FlushConsumers,
    Reinit(u32),
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

pub struct PlayerThreads {
    pub demuxer: Option<JoinHandle<()>>,
    pub audio_decoder: Option<JoinHandle<()>>,
    pub audio_backend: Option<JoinHandle<()>>,
    pub video_decoder: Option<JoinHandle<()>>,
    pub video_output: Option<JoinHandle<()>>,
    pub sub_decoder: Option<JoinHandle<()>>,
}

impl PlayerSession {
    pub fn open(
        path: &str,
        shared: SharedPlayerState,
        external_callback: &ExternalCallback,
        event_tx: Sender<PlayerEvent>,
    ) -> Result<Self, EngineError> {
        let (streams, channels, threads, meta) = open(path, &shared, external_callback, &event_tx)?;
        Ok(Self {
            mode: PlaybackMode::Playing,
            operation: PlaybackOperation::None,
            phase: PlaybackPhase::Active,

            shared,
            external_callback: external_callback.clone(),
            event_tx,
            streams,
            channels,
            threads,
            meta,
        })
    }
    pub fn snapshot(&self) -> PlayerSnapshot {
        PlayerSnapshot {
            mode: self.mode,
            operation: self.operation,
            phase: self.phase,
            active_tracks: self.streams,
        }
    }

    pub fn apply_event(&mut self, event: PlayerEvent) {
        debug!("Event received: {event:?}");
        match event {
            PlayerEvent::Control(event) => self.apply_control(event),
            PlayerEvent::Internal(event) => self.apply_internal(event),
        }
    }

    fn apply_control(&mut self, event: ControlEvent) {
        match event {
            ControlEvent::Play => {
                self.mode = PlaybackMode::Playing;
                self.channels.ab(AbEffect::Output(true));
                self.channels.vo(VoEffect::Output(true));
            }
            ControlEvent::Pause => {
                self.mode = PlaybackMode::Paused;
                self.channels.ab(AbEffect::Output(false));
                self.channels.vo(VoEffect::Output(false));
            }
            ControlEvent::Seek(pts) => {
                self.operation = PlaybackOperation::Seeking(SeekingState::new(pts));
                self.phase = PlaybackPhase::Active;
                self.channels.demuxer(DemuxerEffect::SeekDemuxer(pts));
                self.channels.ab(AbEffect::Output(false));
                self.channels.vo(VoEffect::Output(false));
                if let Some(clock) = &self.channels.audio_clock {
                    let now = get_engine_start().elapsed().as_nanos() as i64;
                    clock.update(pts * 1_000_000, now);
                }
            }
            ControlEvent::AudioMaster(active) => {
                self.shared
                    .clock
                    .active
                    .store(active, std::sync::atomic::Ordering::Relaxed);
            }
            ControlEvent::SelectTrack { kind, id } => {
                if self
                    .meta
                    .get_track(id)
                    .is_none_or(|track| track.kind != kind)
                {
                    return;
                }

                self.operation = PlaybackOperation::SwitchingTrack { kind, id };
                self.phase = PlaybackPhase::Active;
                match kind {
                    TrackKind::Audio => {
                        self.channels.ab(AbEffect::Output(false));
                        self.shared
                            .clock
                            .active
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        self.channels
                            .demuxer(DemuxerEffect::SwitchStream { kind, id });
                    }
                    TrackKind::Video | TrackKind::Subtitle => {
                        self.channels
                            .demuxer(DemuxerEffect::SwitchStream { kind, id });
                    }
                }
            }
            ControlEvent::ChangeVolumes(vol) => {
                self.channels.ab(AbEffect::ModifyVolume(vol));
            }
        }
    }

    fn apply_internal(&mut self, event: InternalEvent) {
        match event {
            InternalEvent::DemuxerSeeked => {
                if self.operation != PlaybackOperation::None {
                    self.phase = PlaybackPhase::Active;
                }
                match self.operation {
                    PlaybackOperation::Seeking(_) => {
                        self.channels.audio(DecoderEffect::Flush);
                        self.channels.video(DecoderEffect::Flush);
                        self.channels.sub(DecoderEffect::Flush);
                    }
                    PlaybackOperation::SwitchingTrack { id, .. } => {
                        if let Some(track) = self.meta.get_track(id) {
                            match track.kind {
                                TrackKind::Audio => {
                                    self.channels.audio(DecoderEffect::Reinit(
                                        track.params.clone(),
                                        track.time_base,
                                    ));
                                    self.channels.audio(DecoderEffect::Flush);
                                }
                                TrackKind::Video => {
                                    self.channels.video(DecoderEffect::Flush);
                                }
                                TrackKind::Subtitle => {
                                    self.channels.sub(DecoderEffect::Reinit(
                                        track.params.clone(),
                                        track.time_base,
                                    ));
                                    self.channels.sub(DecoderEffect::Flush);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            InternalEvent::Flushed(worker) => self.handle_flushed(worker),
            InternalEvent::Synced(worker) => self.handle_synced(worker),
            InternalEvent::Reinitialized { worker, details } => {
                if let (Worker::AudioDecoder, ReinitDetails::Audio { rate }) = (worker, details) {
                    self.channels.ab(AbEffect::Reinit(rate));
                }
            }
            InternalEvent::DemuxerEof => {
                self.phase = PlaybackPhase::InputExhausted;
                if self.operation == PlaybackOperation::None {
                    self.phase = PlaybackPhase::Draining(DrainingState::new());
                    self.channels.audio(DecoderEffect::Drain);
                    self.channels.video(DecoderEffect::Drain);
                }
            }
            InternalEvent::Drained(worker) => {
                if let PlaybackPhase::Draining(draining) = &mut self.phase {
                    draining.set_state(worker, TrackDrainState::Drained);
                    match worker {
                        Worker::AudioDecoder | Worker::VideoDecoder => {
                            self.channels.ab(AbEffect::DrainOutput);
                            self.channels.vo(VoEffect::DrainOutput);
                        }
                        Worker::AudioOutput | Worker::VideoOutput => {
                            match worker {
                                Worker::AudioOutput => {
                                    self.channels.ab(AbEffect::Output(false));
                                }
                                Worker::VideoOutput => {
                                    self.channels.vo(VoEffect::Output(false));
                                }
                                _ => unreachable!(),
                            }
                            if draining.out_drained(&self.streams) {
                                self.phase = PlaybackPhase::Eof;
                                (self.external_callback)(ExternalEvent::Eof);
                            }
                        }
                        Worker::SubDecoder => todo!(),
                    }
                }
            }
        }
    }

    fn handle_flushed(&mut self, worker: Worker) {
        match &mut self.operation {
            PlaybackOperation::Seeking(seeking) => {
                seeking.set_state(worker, TrackSeekState::Flushed);
                match worker {
                    Worker::AudioDecoder | Worker::VideoDecoder => {
                        if seeking.dec_flushed(&self.streams) {
                            self.channels.vo(VoEffect::FlushConsumers);
                            self.channels.ab(AbEffect::FlushConsumers);
                        }
                    }
                    Worker::AudioOutput | Worker::VideoOutput => {
                        if seeking.out_flushed(&self.streams) {
                            self.channels.demuxer(DemuxerEffect::ResumeDemuxer);
                            let sync = SyncMode::Target(seeking.pts);
                            self.channels.audio(DecoderEffect::Sync(sync));
                            self.channels.video(DecoderEffect::Sync(sync));
                            self.channels.sub(DecoderEffect::Sync(sync));
                        }
                    }
                    Worker::SubDecoder => {}
                }
            }
            PlaybackOperation::SwitchingTrack { id, .. } => match worker {
                Worker::AudioDecoder => {
                    self.channels.ab(AbEffect::FlushConsumers);
                }
                Worker::SubDecoder => {
                    self.channels.demuxer(DemuxerEffect::ResumeDemuxer);
                    self.channels
                        .sub(DecoderEffect::Sync(SyncMode::FollowClock));
                }
                Worker::AudioOutput => {
                    self.channels.demuxer(DemuxerEffect::ResumeDemuxer);
                    self.channels
                        .audio(DecoderEffect::Sync(SyncMode::FollowClock));
                }
                Worker::VideoDecoder => {
                    self.channels.vo(VoEffect::FlushConsumers);
                }
                Worker::VideoOutput => {
                    if let Some(track) = self.meta.get_track(*id) {
                        self.channels
                            .video(DecoderEffect::Reinit(track.params.clone(), track.time_base));
                        self.channels
                            .video(DecoderEffect::Sync(SyncMode::FollowClock));
                    }
                    self.channels.demuxer(DemuxerEffect::ResumeDemuxer);
                }
            },
            _ => {}
        }
    }

    fn handle_synced(&mut self, worker: Worker) {
        match &mut self.operation {
            PlaybackOperation::Seeking(seeking) => {
                seeking.set_state(worker, TrackSeekState::Synced);
                if seeking.dec_synced(&self.streams)
                    && matches!(worker, Worker::AudioDecoder | Worker::VideoDecoder)
                {
                    self.operation = PlaybackOperation::None;
                    if self.phase == PlaybackPhase::InputExhausted {
                        self.phase = PlaybackPhase::Draining(DrainingState::new());
                        self.channels.audio(DecoderEffect::Drain);
                        self.channels.video(DecoderEffect::Drain);
                    }
                    match self.mode {
                        PlaybackMode::Playing => {
                            self.channels.ab(AbEffect::Output(true));
                            self.channels.vo(VoEffect::Present);
                            self.channels.vo(VoEffect::Output(true));
                        }
                        PlaybackMode::Paused => {
                            self.channels.vo(VoEffect::Present);
                        }
                    }
                }
            }
            PlaybackOperation::SwitchingTrack { kind, id } => {
                match worker {
                    Worker::AudioDecoder => {
                        self.shared
                            .clock
                            .active
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        self.streams.audio = Some(*id);
                        if self.mode == PlaybackMode::Playing {
                            self.channels.ab(AbEffect::Output(true));
                        }
                        (self.external_callback)(ExternalEvent::TrackChanged(*kind, *id));
                        self.operation = PlaybackOperation::None;
                    }
                    Worker::VideoDecoder => {
                        self.channels.vo(VoEffect::Present);
                        self.streams.video = Some(*id);
                        (self.external_callback)(ExternalEvent::TrackChanged(*kind, *id));
                        self.operation = PlaybackOperation::None;
                    }
                    Worker::SubDecoder => {
                        self.streams.subs = Some(*id);
                        (self.external_callback)(ExternalEvent::TrackChanged(*kind, *id));
                        self.operation = PlaybackOperation::None;
                    }
                    _ => {}
                }
                if self.operation == PlaybackOperation::None
                    && self.phase == PlaybackPhase::InputExhausted
                {
                    self.phase = PlaybackPhase::Draining(DrainingState::new());
                    self.channels.audio(DecoderEffect::Drain);
                    self.channels.video(DecoderEffect::Drain);
                }
            }
            _ => {}
        }
    }
}

impl std::fmt::Debug for DecoderEffect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecoderEffect::Flush => write!(f, "Flush"),
            DecoderEffect::Sync(mode) => write!(f, "Sync({mode:?})"),
            DecoderEffect::Drain => write!(f, "Drain"),
            DecoderEffect::Reinit(_, _) => write!(f, "Reinit"),
        }
    }
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

impl Drop for PlayerThreads {
    fn drop(&mut self) {
        if let Some(thread) = self.demuxer.take() {
            thread.join().ok();
        }
        if let Some(thread) = self.audio_decoder.take() {
            thread.join().ok();
        }
        if let Some(thread) = self.audio_backend.take() {
            thread.join().ok();
        }
        if let Some(thread) = self.video_decoder.take() {
            thread.join().ok();
        }
        if let Some(thread) = self.sub_decoder.take() {
            thread.join().ok();
        }
        if let Some(thread) = self.video_output.take() {
            thread.join().ok();
        }
    }
}

fn open(
    path: &str,
    shared: &SharedPlayerState,
    external_callback: &ExternalCallback,
    event_tx: &Sender<PlayerEvent>,
) -> Result<(ActiveTracks, PlayerChannels, PlayerThreads, PlayerMeta), EngineError> {
    let ictx = input(&path)?;
    let mut channels = PlayerChannels::new();
    let mut threads = PlayerThreads::new();
    channels.external_callback = Some(external_callback.clone());

    let audio_stream = ictx.streams().best(Type::Audio);
    let video_stream = ictx.streams().best(Type::Video);
    let sub_stream = ictx.streams().best(Type::Subtitle);

    let meta = PlayerMeta::new(&ictx);

    let mut audio_idx = None;
    let mut video_idx = None;
    let mut sub_idx = None;

    if let Some(audio_stream) = audio_stream {
        audio_idx = Some(audio_stream.index());
        let (dec_handle, ab_handle, dec_effect_tx, pw_tx, packet_tx) = spawn_audio_stream(
            &audio_stream,
            event_tx,
            external_callback,
            &shared.audio_info.volume,
            shared.clock.clone(),
            path,
        );

        channels.audio_tx = Some(dec_effect_tx);
        channels.pw_tx = Some(pw_tx);
        channels.audio_packet_tx = Some(packet_tx);
        channels.audio_clock = Some(shared.clock.clone());
        shared.clock.active.store(true, Ordering::Relaxed);
        threads.audio_decoder = Some(dec_handle);
        threads.audio_backend = Some(ab_handle);
    }

    if let Some(video_stream) = video_stream {
        video_idx = Some(video_stream.index());
        let (dec_handle, output_handle, dec_tx, vo_tx, packet_tx) = spawn_video_stream(
            &video_stream,
            event_tx,
            Some(external_callback),
            &shared.config,
            &shared.video_output.frame,
            &shared.video_output.current_pts,
            &shared.clock,
        );
        channels.video_tx = Some(dec_tx);
        channels.video_output_tx = Some(vo_tx);
        channels.video_packet_tx = Some(packet_tx);
        threads.video_decoder = Some(dec_handle);
        threads.video_output = Some(output_handle);
    }

    if let Some(sub_stream) = sub_stream {
        sub_idx = Some(sub_stream.index());
        let (dec_handle, sub_tx, packet_tx) = spawn_sub_stream(
            &ictx,
            event_tx,
            &sub_stream,
            shared.clock.clone(),
            shared.sub_output.track.clone(),
            shared.sub_output.renderer.clone(),
        );
        channels.sub_tx = Some(sub_tx);
        channels.sub_packet_tx = Some(packet_tx);
        threads.sub_decoder = Some(dec_handle);
    }

    let demuxer_event_tx = event_tx.clone();
    let (demuxer_tx, demuxer_rx) = crossbeam_channel::unbounded();
    let demuxer_waker = Arc::new(ThreadWaker::new());
    let memory_budget = MemoryBudget::new(150 * 1024 * 1024, demuxer_waker.clone());
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
        shared.clock.clone(),
        demuxer_waker,
        memory_budget,
    );
    let demuxer_handle = std::thread::Builder::new()
        .name("demuxer".into())
        .spawn(move || {
            demuxer.waker.set();
            demuxer.read_packets();
        })
        .unwrap();
    threads.demuxer = Some(demuxer_handle);

    Ok((
        ActiveTracks {
            subs: sub_idx,
            audio: audio_idx,
            video: video_idx,
        },
        channels,
        threads,
        meta,
    ))
}
