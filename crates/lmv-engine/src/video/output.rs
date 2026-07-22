use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use tracing::debug;

use crate::{
    ExternalEvent,
    PlayerEvent::{self, Internal},
    VideoFrame,
    engine::ExternalCallback,
    session::{
        InternalEvent::{FramesDrained, VideoOutputFlushed},
        VoEffect,
    },
    utils::Clock,
};

pub struct VideoOutput {
    audio_clock: Arc<Clock>,
    frame_rx: Receiver<VideoFrame>,
    frame: Arc<Mutex<Option<VideoFrame>>>,
    current_pts: Arc<AtomicI64>,
    next_frame: Option<VideoFrame>,
    effect_rx: Receiver<VoEffect>,
    event_tx: Sender<PlayerEvent>,
    callback: Option<ExternalCallback>,
    state: OutputState,
    last_pts: Option<i64>,
    drain: bool,
}

enum OutputState {
    Active,
    Idle,
}

enum StageResult {
    Deadline(Instant),
    Empty,
    Drained,
    Disconnected,
}

impl VideoOutput {
    pub fn new(
        audio_clock: Arc<Clock>,
        frame_rx: Receiver<VideoFrame>,
        frame: Arc<Mutex<Option<VideoFrame>>>,
        current_pts: Arc<AtomicI64>,
        effect_rx: Receiver<VoEffect>,
        event_tx: Sender<PlayerEvent>,
        callback: Option<ExternalCallback>,
    ) -> Self {
        Self {
            audio_clock,
            frame_rx,
            frame,
            current_pts,
            next_frame: None,
            effect_rx,
            event_tx,
            callback,
            state: OutputState::Active,
            last_pts: None,
            drain: false,
        }
    }

    pub fn process(&mut self) {
        loop {
            match &mut self.state {
                OutputState::Active => match self.stage_next() {
                    StageResult::Deadline(deadline) => {
                        // TODO render subs here instead of in the widget i'm waiting till we have track switching for this
                        if deadline > Instant::now() {
                            match self.effect_rx.recv_deadline(deadline) {
                                Ok(effect) => {
                                    self.effect_recv(effect);
                                    continue;
                                }
                                Err(RecvTimeoutError::Timeout) => {}
                                Err(RecvTimeoutError::Disconnected) => {
                                    self.state = OutputState::Idle;
                                    continue;
                                }
                            }
                        }
                        if let Some(frame) = self.next_frame.take() {
                            let audio_ms = self.audio_clock.get_ms();
                            let next_pts = frame.info.pts.unwrap();
                            let av_offset = next_pts - audio_ms;
                            debug!("audio ms: {audio_ms}, av_offset: {av_offset}");
                            self.present_frame(frame);
                        }
                    }
                    StageResult::Empty => {
                        if let Ok(effect) = self.effect_rx.recv_timeout(Duration::from_millis(1)) {
                            self.effect_recv(effect);
                        }
                    }
                    StageResult::Drained | StageResult::Disconnected => {
                        self.state = OutputState::Idle;
                    }
                },
                OutputState::Idle => {
                    let effect = self.effect_rx.recv().unwrap();
                    self.effect_recv(effect);
                }
            }
        }
    }

    fn stage_next(&mut self) -> StageResult {
        if !self.audio_clock.active.load(Ordering::Relaxed) {
            return StageResult::Disconnected; // TODO later we wont rely on the audio for video only files
        }

        let mut next_frame = {
            if let Some(f) = self.next_frame.take() {
                f
            } else {
                match self.frame_rx.try_recv() {
                    Ok(f) => f,
                    Err(TryRecvError::Empty) => {
                        if self.drain {
                            let _ = self.event_tx.send(Internal(FramesDrained));
                            self.drain = false;
                            return StageResult::Drained;
                        }
                        return StageResult::Empty;
                    }
                    Err(TryRecvError::Disconnected) => return StageResult::Disconnected,
                }
            }
        };

        loop {
            let audio_ms = self.audio_clock.get_ms();
            let next_pts = next_frame.info.pts.unwrap();
            let av_offset = next_pts - audio_ms;

            let frame_duration = self
                .last_pts
                .map_or(41, |prev| (next_pts - prev).abs().max(1));
            self.last_pts = Some(next_pts);

            if av_offset < -frame_duration {
                match self.frame_rx.try_recv() {
                    Ok(f) => {
                        next_frame = f;
                        continue;
                    }
                    Err(TryRecvError::Empty) => {
                        if self.drain {
                            let _ = self.event_tx.send(Internal(FramesDrained));
                            self.drain = false;
                            return StageResult::Drained;
                        }
                        return StageResult::Empty;
                    }
                    Err(_) => {
                        return StageResult::Disconnected;
                    }
                }
            }

            let max_offset = (frame_duration + frame_duration / 2) as u64;
            let delay_ms = av_offset.max(0) as u64;
            let deadline = Instant::now() + Duration::from_millis(delay_ms.min(max_offset));

            self.next_frame = Some(next_frame);
            return StageResult::Deadline(deadline);
        }
    }

    fn present_frame(&self, frame: VideoFrame) {
        let pts = frame.info.pts.unwrap_or(0);
        self.current_pts.store(pts, Ordering::Relaxed);
        *self.frame.lock().unwrap() = Some(frame);
        if let Some(cb) = &self.callback {
            cb(ExternalEvent::NewFrame);
        }
    }

    fn effect_recv(&mut self, effect: VoEffect) {
        match effect {
            VoEffect::Output(output) => {
                if output {
                    self.state = OutputState::Active;
                } else {
                    self.state = OutputState::Idle;
                }
            }
            VoEffect::Present => {
                self.next_frame = None;
                if let Ok(current_frame) = self.frame_rx.recv() {
                    self.present_frame(current_frame);
                }
            }
            VoEffect::FlushConsumers => {
                while self.frame_rx.try_recv().is_ok() {}
                self.next_frame = None;
                self.last_pts = None;
                let _ = self.event_tx.send(Internal(VideoOutputFlushed));
            }
            VoEffect::DrainOutput => {
                self.drain = true;
            }
        }
    }
}
