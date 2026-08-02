use std::{
    collections::VecDeque,
    sync::{Arc, atomic::Ordering::Relaxed},
    thread::JoinHandle,
};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, bounded};
use ffmpeg_next::{Rational, codec::Parameters};
use tracing::debug;

mod messages;
mod metadata;
mod open;
mod state;

pub(crate) use messages::SessionChannels;
pub use messages::{
    AbEffect, ControlEvent, DecoderEffect, DemuxerEffect, InternalEvent, PlayerEvent,
    ReinitDetails, SyncMode, VoEffect,
};
pub use metadata::{PlayerMeta, TrackId, TrackKind, TrackMeta};
pub use state::{
    ActiveTracks, DrainingState, PlaybackMode, PlaybackOperation, PlaybackPhase, SeekingState,
    SessionState, TrackDrainState, TrackSeekState, Worker,
};

use crate::{
    ExternalEvent, SharedPlayerState,
    engine::{EngineError, ExternalCallback, get_engine_start},
    session::{
        TrackSeekState::{Flushed, Synced},
        open::open,
    },
};
pub type Pts = state::Pts;

pub struct PlayerSession {
    pub state: SessionState,
    pub shared: SharedPlayerState,
    pub channels: SessionChannels,
    op_queue: VecDeque<PlayerEvent>,
    pub threads: PlayerThreads,
    pub meta: PlayerMeta,
    snapshot_state: Arc<ArcSwap<SessionState>>,
}

#[derive(Default)]
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
        snapshot_state: Arc<ArcSwap<SessionState>>,
    ) -> Result<Self, EngineError> {
        let (streams, channels, threads, meta) = open(path, &shared, external_callback, &event_tx)?;
        let session = Self {
            state: SessionState::new(streams),
            shared,
            channels,
            op_queue: VecDeque::new(),
            threads,
            meta,
            snapshot_state,
        };
        session.snapshot();
        Ok(session)
    }

    pub fn snapshot(&self) {
        self.snapshot_state.store(Arc::new(self.state));
    }

    pub fn apply_event(&mut self, event: PlayerEvent, event_rx: &Receiver<PlayerEvent>) {
        debug!("Event received: {event:?}");
        match event {
            PlayerEvent::Control(event) => self.apply_control(event, event_rx),
            PlayerEvent::Internal(event) => self.apply_internal(event),
        }
    }

    fn apply_control(&mut self, event: ControlEvent, event_rx: &Receiver<PlayerEvent>) {
        match event {
            ControlEvent::Play => {
                self.state.mode = PlaybackMode::Playing;
                self.snapshot();
                self.channels.ab(AbEffect::Output(true));
                self.channels.vo(VoEffect::Output(true));
            }
            ControlEvent::Pause => {
                self.state.mode = PlaybackMode::Paused;
                self.snapshot();
                self.channels.ab(AbEffect::Output(false));
                self.channels.vo(VoEffect::Output(false));
            }
            ControlEvent::ChangeVolumes(vol) => {
                self.channels.ab(AbEffect::ModifyVolume(vol));
            }
            ControlEvent::AudioMaster(active) => {
                self.shared
                    .clock
                    .active
                    .store(active, std::sync::atomic::Ordering::Relaxed);
            }
            ControlEvent::Seek(pts) => {
                if self.state.is_busy() {
                    debug!("Adding {event:?} to the queue");
                    self.op_queue.push_back(PlayerEvent::Control(event));
                    return;
                }
                self.seek(event_rx, pts);
            }
            ControlEvent::SelectTrack { id, kind } => {
                if self.state.is_busy() {
                    self.op_queue.push_back(PlayerEvent::Control(event));
                    return;
                }
                let Some(track) = self.meta.get_track(id) else {
                    return;
                };
                if track.kind != kind {
                    return;
                }
                let params = track.params.clone();
                let time_base = track.time_base;
                self.switch_track(event_rx, id, kind, params, time_base);
            }
        }
        if !self.state.is_busy()
            && let Some(event) = self.op_queue.pop_front()
        {
            self.apply_event(event, event_rx);
        }
    }

    fn seek(&mut self, event_rx: &Receiver<PlayerEvent>, pts: i64) {
        let mut state = SeekingState::new(pts);
        self.state.operation = PlaybackOperation::Seeking(state);
        self.snapshot();
        self.channels.ab(AbEffect::Output(false));
        self.channels.vo(VoEffect::Output(false));
        let now = get_engine_start().elapsed().as_nanos() as i64;
        self.shared.clock.update(pts * 1_000_000, now);

        let (seek_tx, seek_rx) = bounded(1);
        self.channels
            .demuxer(DemuxerEffect::SeekDemuxer(seek_tx, pts));
        self.wait_for(event_rx, &seek_rx);
        self.state.phase = PlaybackPhase::Active;
        self.snapshot();

        let (dec_flush_tx, dec_flush_rx) = bounded(1);
        self.channels
            .audio(DecoderEffect::Flush(dec_flush_tx.clone()));
        self.channels
            .video(DecoderEffect::Flush(dec_flush_tx.clone()));
        self.channels
            .sub(DecoderEffect::Flush(dec_flush_tx.clone()));

        while !state.dec_flushed(&self.state.tracks) {
            let resp = self.wait_for(event_rx, &dec_flush_rx);
            debug!(?resp);
            state.set_state(resp, Flushed);
            self.state.operation = PlaybackOperation::Seeking(state);
            self.snapshot();
        }

        let (con_flush_tx, con_flush_rx) = bounded(1);
        self.channels
            .ab(AbEffect::FlushConsumers(con_flush_tx.clone()));
        self.channels
            .vo(VoEffect::FlushConsumers(con_flush_tx.clone()));

        while !state.out_flushed(&self.state.tracks) {
            let resp = self.wait_for(event_rx, &con_flush_rx);
            debug!(?resp);
            state.set_state(resp, Flushed);
            self.state.operation = PlaybackOperation::Seeking(state);
            self.snapshot();
        }

        let (sync_tx, sync_rx) = bounded(3);

        let sync = SyncMode::Target(pts);
        self.channels
            .audio(DecoderEffect::Sync(sync_tx.clone(), sync));
        self.channels
            .video(DecoderEffect::Sync(sync_tx.clone(), sync));
        self.channels.sub(DecoderEffect::Sync(sync_tx, sync));
        self.channels.demuxer(DemuxerEffect::ResumeDemuxer);

        while !state.dec_synced(&self.state.tracks) {
            let resp = self.wait_for(event_rx, &sync_rx);
            debug!(?resp);
            state.set_state(resp, Synced);
            self.state.operation = PlaybackOperation::Seeking(state);
            self.snapshot();
        }

        self.state.operation = PlaybackOperation::None;
        if self.state.phase == PlaybackPhase::InputExhausted {
            self.state.phase = PlaybackPhase::Draining(DrainingState::new());
            self.channels.audio(DecoderEffect::Drain);
            self.channels.video(DecoderEffect::Drain);
        }
        self.snapshot();

        match self.state.mode {
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

    fn switch_track(
        &mut self,
        event_rx: &Receiver<PlayerEvent>,
        id: TrackId,
        kind: TrackKind,
        params: Parameters,
        time_base: Rational,
    ) {
        self.state.operation = PlaybackOperation::SwitchingTrack { kind, id };
        self.snapshot();

        match kind {
            TrackKind::Audio => {
                self.shared.clock.active.store(false, Relaxed);
                self.channels.ab(AbEffect::Output(false));
            }
            TrackKind::Video | TrackKind::Subtitle => {}
        }

        let (seek_tx, seek_rx) = bounded(1);

        self.channels
            .demuxer(DemuxerEffect::SwitchStream { seek_tx, kind, id });
        self.wait_for(event_rx, &seek_rx);
        self.state.phase = PlaybackPhase::Active;
        self.snapshot();

        match kind {
            TrackKind::Audio => {
                let (dec_flush_tx, dec_flush_rx) = bounded(1);

                self.channels.audio(DecoderEffect::Flush(dec_flush_tx));
                self.wait_for(event_rx, &dec_flush_rx);

                let (out_flush_tx, out_flush_rx) = bounded(1);

                self.channels.ab(AbEffect::FlushConsumers(out_flush_tx));
                self.wait_for(event_rx, &out_flush_rx);

                let (dec_reinit_tx, dec_reinit_rx) = bounded(1);

                self.channels
                    .audio(DecoderEffect::Reinit(dec_reinit_tx, params, time_base));
                let resp = self.wait_for(event_rx, &dec_reinit_rx);
                let ReinitDetails::Audio { rate } = resp.1 else {
                    unreachable!("Audio decoder returned invalid details");
                };

                self.channels.ab(AbEffect::Reinit(rate));

                let (sync_tx, sync_rx) = bounded(1);

                let sync = SyncMode::FollowClock;
                self.channels.audio(DecoderEffect::Sync(sync_tx, sync));
                self.channels.demuxer(DemuxerEffect::ResumeDemuxer);
                self.wait_for(event_rx, &sync_rx);

                self.shared
                    .clock
                    .active
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.state.tracks.audio = Some(id);
                if self.state.mode == PlaybackMode::Playing {
                    self.channels.ab(AbEffect::Output(true));
                }
                self.state.operation = PlaybackOperation::None;
                if self.state.phase == PlaybackPhase::InputExhausted {
                    self.state.phase = PlaybackPhase::Draining(DrainingState::new());
                    self.channels.audio(DecoderEffect::Drain);
                    self.channels.video(DecoderEffect::Drain);
                }
                self.snapshot();
                (self.channels.external_callback)(ExternalEvent::TrackChanged(kind, id));
            }
            TrackKind::Video => {
                let (dec_flush_tx, dec_flush_rx) = bounded(1);

                self.channels.video(DecoderEffect::Flush(dec_flush_tx));
                self.wait_for(event_rx, &dec_flush_rx);

                let (out_flush_tx, out_flush_rx) = bounded(1);

                self.channels.vo(VoEffect::FlushConsumers(out_flush_tx));
                self.wait_for(event_rx, &out_flush_rx);

                let (dec_reinit_tx, dec_reinit_rx) = bounded(1);

                self.channels.video(DecoderEffect::Reinit(
                    dec_reinit_tx,
                    params.clone(),
                    time_base,
                ));
                self.wait_for(event_rx, &dec_reinit_rx);

                let (sync_tx, sync_rx) = bounded(1);

                let sync = SyncMode::FollowClock;
                self.channels.video(DecoderEffect::Sync(sync_tx, sync));
                self.channels.demuxer(DemuxerEffect::ResumeDemuxer);
                self.wait_for(event_rx, &sync_rx);

                self.state.tracks.video = Some(id);
                if self.state.mode == PlaybackMode::Paused {
                    self.channels.vo(VoEffect::Present);
                }
                self.state.operation = PlaybackOperation::None;
                if self.state.phase == PlaybackPhase::InputExhausted {
                    self.state.phase = PlaybackPhase::Draining(DrainingState::new());
                    self.channels.audio(DecoderEffect::Drain);
                    self.channels.video(DecoderEffect::Drain);
                }
                self.snapshot();
                (self.channels.external_callback)(ExternalEvent::TrackChanged(kind, id));
            }
            TrackKind::Subtitle => {
                let (dec_flush_tx, dec_flush_rx) = bounded(1);

                self.channels.sub(DecoderEffect::Flush(dec_flush_tx));
                self.wait_for(event_rx, &dec_flush_rx);

                let (dec_reinit_tx, dec_reinit_rx) = bounded(1);

                self.channels.sub(DecoderEffect::Reinit(
                    dec_reinit_tx,
                    params.clone(),
                    time_base,
                ));
                self.wait_for(event_rx, &dec_reinit_rx);

                let (sync_tx, sync_rx) = bounded(1);

                let sync = SyncMode::FollowClock;
                self.channels.sub(DecoderEffect::Sync(sync_tx, sync));
                self.channels.demuxer(DemuxerEffect::ResumeDemuxer);
                self.wait_for(event_rx, &sync_rx);

                self.state.tracks.subs = Some(id);
                self.state.operation = PlaybackOperation::None;
                if self.state.phase == PlaybackPhase::InputExhausted {
                    self.state.phase = PlaybackPhase::Draining(DrainingState::new());
                    self.channels.audio(DecoderEffect::Drain);
                    self.channels.video(DecoderEffect::Drain);
                }
                self.snapshot();
                (self.channels.external_callback)(ExternalEvent::TrackChanged(kind, id));
            }
        }
        if !self.state.is_busy() && self.state.phase == PlaybackPhase::InputExhausted {
            self.state.phase = PlaybackPhase::Draining(DrainingState::new());
            self.snapshot();
            self.channels.audio(DecoderEffect::Drain);
            self.channels.video(DecoderEffect::Drain);
        }
    }

    fn apply_internal(&mut self, event: InternalEvent) {
        match event {
            InternalEvent::DemuxerEof => {
                self.state.phase = PlaybackPhase::InputExhausted;
                if !self.state.is_busy() {
                    self.state.phase = PlaybackPhase::Draining(DrainingState::new());
                    self.channels.audio(DecoderEffect::Drain);
                    self.channels.video(DecoderEffect::Drain);
                }
                self.snapshot();
            }
            InternalEvent::Drained(worker) => {
                if !matches!(self.state.phase, PlaybackPhase::Draining(_)) {
                    return;
                }
                if let PlaybackPhase::Draining(draining) = &mut self.state.phase {
                    draining.set_state(worker, TrackDrainState::Drained);
                }
                self.snapshot();

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
                        let out_drained = match &self.state.phase {
                            PlaybackPhase::Draining(draining) => {
                                draining.out_drained(&self.state.tracks)
                            }
                            _ => unreachable!(),
                        };
                        if out_drained {
                            self.state.phase = PlaybackPhase::Eof;
                            self.snapshot();
                            (self.channels.external_callback)(ExternalEvent::Eof);
                        }
                    }
                    Worker::SubDecoder => todo!(),
                }
            }
        }
    }

    fn wait_for<T>(&mut self, event_rx: &Receiver<PlayerEvent>, response_rx: &Receiver<T>) -> T {
        loop {
            crossbeam_channel::select! {
                recv(event_rx) -> event => {
                    self.apply_event(event.unwrap(), event_rx);
                },
                recv(response_rx) -> response => {
                    return response.unwrap();
                },
            }
        }
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
