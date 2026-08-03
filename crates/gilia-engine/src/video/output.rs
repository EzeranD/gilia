// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(not(target_os = "windows"))]
use crossbeam_channel::RecvTimeoutError;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
#[cfg(target_os = "windows")]
use gilia_windows::{EventRecvTimeoutError as RecvTimeoutError, MmcssRegistration, w};
use tracing::debug;

use crate::{
    ExternalEvent,
    PlayerEvent::{self, Internal},
    VideoFrame,
    engine::{ExternalCallback, get_engine_start},
    session::{InternalEvent::Drained, VoEffect, Worker},
    utils::Clock,
    video::VideoOutputReceiver,
};

pub struct VideoOutput {
    audio_clock: Arc<Clock>,
    frame_rx: Receiver<VideoFrame>,
    frame: Arc<Mutex<Option<VideoFrame>>>,
    current_pts: Arc<AtomicI64>,
    next_frame: Option<VideoFrame>,
    effect_rx: VideoOutputReceiver,
    event_tx: Sender<PlayerEvent>,
    callback: ExternalCallback,
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
        effect_rx: VideoOutputReceiver,
        event_tx: Sender<PlayerEvent>,
        callback: ExternalCallback,
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
        #[cfg(target_os = "windows")]
        let _mmcss = MmcssRegistration::register(w!("Playback"));

        loop {
            match self.state {
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
                                Err(RecvTimeoutError::Disconnected) => return,
                                #[cfg(target_os = "windows")]
                                Err(RecvTimeoutError::Sync { source }) => return,
                            }
                        }
                        if let Some(frame) = self.next_frame.take() {
                            let audio_ms = self.audio_clock.get_ms();
                            let next_pts = frame.info.pts.unwrap();
                            let av_offset = next_pts - audio_ms;
                            debug!("current_ms: {audio_ms}, av_offset: {av_offset}");
                            self.present_frame(frame);
                        }
                    }
                    StageResult::Empty => {
                        match self.effect_rx.recv_timeout(Duration::from_millis(1)) {
                            Ok(effect) => self.effect_recv(effect),
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => return,
                            #[cfg(target_os = "windows")]
                            Err(RecvTimeoutError::Sync { source }) => return,
                        }
                    }
                    StageResult::Drained | StageResult::Disconnected => {
                        self.state = OutputState::Idle;
                    }
                },
                OutputState::Idle => match self.effect_rx.recv() {
                    Ok(effect) => self.effect_recv(effect),
                    Err(_) => return,
                },
            }
        }
    }

    fn stage_next(&mut self) -> StageResult {
        let mut next_frame = {
            if let Some(f) = self.next_frame.take() {
                f
            } else {
                match self.frame_rx.try_recv() {
                    Ok(f) => f,
                    Err(TryRecvError::Empty) => {
                        if self.drain {
                            let _ = self.event_tx.send(Internal(Drained(Worker::VideoOutput)));
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
                            let _ = self.event_tx.send(Internal(Drained(Worker::VideoOutput)));
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
        if !self.audio_clock.active.load(Ordering::Relaxed) {
            let now = get_engine_start().elapsed().as_nanos() as i64;
            self.audio_clock.update(pts * 1_000_000, now);
        }
        *self.frame.lock().unwrap() = Some(frame);
        (self.callback)(ExternalEvent::NewFrame);
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
            VoEffect::FlushConsumers(flush_tx) => {
                while self.frame_rx.try_recv().is_ok() {}
                self.next_frame = None;
                self.last_pts = None;
                self.drain = false;
                let _ = flush_tx.send(Worker::VideoOutput);
            }
            VoEffect::DrainOutput => {
                self.drain = true;
            }
        }
    }
}
