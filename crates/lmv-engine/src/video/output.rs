use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
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

const THRESHOLD: i64 = 20;

pub struct VideoOutput {
    audio_clock: Arc<Clock>,
    frame_rx: Receiver<VideoFrame>,
    frame: Arc<Mutex<Option<VideoFrame>>>,
    current_pts: Arc<AtomicI64>,
    next_frame: Option<VideoFrame>,
    drain: bool,
    state: OutputState,
    effect_rx: Receiver<VoEffect>,
    event_tx: Sender<PlayerEvent>,
    callback: Option<ExternalCallback>,
}

enum OutputState {
    Active,
    Idle,
}

enum PresentResult {
    Delay(Duration),
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
            drain: false,
            state: OutputState::Active,
            effect_rx,
            event_tx,
            callback,
        }
    }

    pub fn process(&mut self) {
        loop {
            while let Ok(effect) = self.effect_rx.try_recv() {
                self.effect_recv(effect);
            }

            match &mut self.state {
                OutputState::Active => match self.present_next() {
                    PresentResult::Delay(duration) => {
                        std::thread::park_timeout(duration);
                    }
                    PresentResult::Empty => {
                        std::thread::park_timeout(Duration::from_millis(1));
                    }
                    PresentResult::Drained | PresentResult::Disconnected => {
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

    fn present_next(&mut self) -> PresentResult {
        if !self.audio_clock.active.load(Ordering::Relaxed) {
            return PresentResult::Disconnected; // TODO later we wont rely on the audio for video only files
        }

        let audio_ms = self.audio_clock.get_ms();
        let mut current_frame = {
            if let Some(f) = self.next_frame.take() {
                f
            } else {
                match self.frame_rx.try_recv() {
                    Ok(f) => f,
                    Err(TryRecvError::Empty) => {
                        if self.drain {
                            let _ = self.event_tx.send(Internal(FramesDrained));
                            self.drain = false;
                            return PresentResult::Drained;
                        }
                        return PresentResult::Empty;
                    }
                    Err(TryRecvError::Disconnected) => return PresentResult::Disconnected,
                }
            }
        };

        loop {
            let current_pts = current_frame.info.pts.unwrap();
            let av_offset = current_pts - audio_ms;
            debug!("audio ms: {audio_ms}, av_offset: {av_offset}");

            if av_offset > THRESHOLD {
                self.next_frame = Some(current_frame);
                return PresentResult::Delay(Duration::from_millis((av_offset - THRESHOLD) as u64));
            }

            if av_offset < -THRESHOLD {
                match self.frame_rx.try_recv() {
                    Ok(f) => {
                        current_frame = f;
                        continue;
                    }
                    Err(TryRecvError::Empty) => {
                        if self.drain {
                            let _ = self.event_tx.send(Internal(FramesDrained));
                            self.drain = false;
                            return PresentResult::Drained;
                        }
                        return PresentResult::Empty;
                    }
                    Err(_) => {
                        self.present_frame(current_frame);
                        return PresentResult::Disconnected;
                    }
                }
            }

            let next_frame = {
                match self.frame_rx.try_recv() {
                    Ok(f) => f,
                    Err(TryRecvError::Empty) => {
                        if self.drain {
                            let _ = self.event_tx.send(Internal(FramesDrained));
                            self.drain = false;
                            return PresentResult::Drained;
                        }
                        return PresentResult::Empty;
                    }
                    Err(_) => {
                        self.present_frame(current_frame);
                        return PresentResult::Disconnected;
                    }
                }
            };
            let next_error = next_frame.info.pts.unwrap() - audio_ms;
            if next_error <= THRESHOLD {
                current_frame = next_frame;
            } else {
                self.next_frame = Some(next_frame);
                self.present_frame(current_frame);
                return PresentResult::Delay(Duration::from_millis(next_error as u64));
            }
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
                if let Ok(current_frame) = self.frame_rx.recv() {
                    self.present_frame(current_frame);
                }
            }
            VoEffect::FlushConsumers => {
                while self.frame_rx.try_recv().is_ok() {}
                self.next_frame = None;
                let _ = self.event_tx.send(Internal(VideoOutputFlushed));
            }
            VoEffect::DrainOutput => {
                self.drain = true;
            }
        }
    }
}
