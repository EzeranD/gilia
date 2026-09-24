// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError, select_biased};
use ffmpeg_next::{
    Packet, Rational, Rescale,
    codec::{Context, Parameters},
    ffi as ffmsys, frame,
};
use tracing::{debug, error};

pub use crate::video::frame::{DecoderMode, VideoFrame, wrap_frame};
use crate::{
    PlayerEvent::Internal,
    engine::EngineConfig,
    session::{
        DecoderEffect, InternalEvent::Drained, PlayerEvent, ReinitDetails, SyncMode, Worker,
    },
    utils::{Clock, TrackedPacket},
    video::hw_ffmpeg::{HwOption, ManagedVideo, create_decoder, get_hw_options},
};

pub struct VideoDecoder {
    ctx: VideoContext,
    state: DecoderState,
}

#[derive(Debug)]
pub struct PendingSync {
    tx: Sender<Worker>,
    mode: SyncMode,
}

pub struct VideoContext {
    config: Arc<EngineConfig>,
    clock: Arc<Clock>,
    decoder: Option<ManagedVideo>,
    mode: DecoderMode,
    parameters: Parameters,
    packet_tx: Receiver<TrackedPacket>,
    frame_tx: Sender<VideoFrame>,
    effect_rx: Receiver<DecoderEffect>,
    event_tx: Sender<PlayerEvent>,
    time_base: Rational,
    printed: bool,
}

pub enum DecoderState {
    Creating {
        packets: Vec<TrackedPacket>,
        sync: Option<PendingSync>,
    },
    Active,
    Syncing(Sender<Worker>, SyncMode),
    Draining,
    Idle,
}

impl VideoDecoder {
    #[must_use]
    pub fn new(
        config: Arc<EngineConfig>,
        parameters: Parameters,
        packet_tx: Receiver<TrackedPacket>,
        frame_tx: Sender<VideoFrame>,
        time_base: Rational,
        effect_rx: Receiver<DecoderEffect>,
        event_tx: Sender<PlayerEvent>,
        clock: Arc<Clock>,
    ) -> Self {
        let ctx = VideoContext {
            config,
            clock,
            decoder: None,
            mode: DecoderMode::Hw,
            parameters,
            packet_tx,
            frame_tx,
            time_base,
            effect_rx,
            event_tx,
            printed: false,
        };
        Self {
            ctx,
            state: DecoderState::Creating {
                packets: Vec::new(),
                sync: None,
            },
        }
    }

    pub fn process(&mut self) {
        let VideoDecoder { ctx, state } = self;
        loop {
            match state {
                DecoderState::Creating { packets, sync } => {
                    ctx.init_decoder(packets, sync.as_ref());
                    *state = DecoderState::Active;
                    debug!("State changed: {:?}", state);
                }
                DecoderState::Active => {
                    select_biased! {
                        recv(ctx.effect_rx) -> effect => {
                            let effect = effect.unwrap();
                            ctx.effect_recv(effect, state);
                        }
                        recv(ctx.packet_tx) -> packet => ctx.decode(state, &packet.unwrap())
                    }
                }
                DecoderState::Syncing(_, _) => {
                    select_biased! {
                        recv(ctx.effect_rx) -> effect => {
                            let effect = effect.unwrap();
                            ctx.effect_recv(effect, state);
                        }
                        recv(ctx.packet_tx) -> packet =>
                            ctx.sync(state, &packet.unwrap())
                    }
                }
                DecoderState::Draining => {
                    match ctx.effect_rx.try_recv() {
                        Ok(effect) => {
                            ctx.effect_recv(effect, state);
                            continue;
                        }
                        Err(TryRecvError::Disconnected) => return,
                        Err(TryRecvError::Empty) => {}
                    }

                    match ctx.packet_tx.try_recv() {
                        Ok(packet) => ctx.decode(state, &packet),
                        Err(TryRecvError::Empty) => {
                            let _ = ctx.event_tx.send(Internal(Drained(Worker::VideoDecoder)));
                            *state = DecoderState::Idle;
                            debug!("State changed: {:?}", state);
                        }
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                DecoderState::Idle => {
                    let effect = ctx.effect_rx.recv().unwrap();
                    ctx.effect_recv(effect, state);
                }
            }
        }
    }
}

impl VideoContext {
    fn init_decoder(
        &mut self,
        packets: &mut Vec<TrackedPacket>,
        pending_sync: Option<&PendingSync>,
    ) {
        if self.config.hw_dec {
            let parameters = self.parameters.clone();
            let mut hw_options = get_hw_options(&parameters);
            for hw_option in &mut hw_options {
                debug!("hw_cfg: {:?}", hw_option.hw_cfg);
                if let Some(decoder) = self.init_hw(hw_option, packets, pending_sync) {
                    self.decoder = Some(decoder);
                    break;
                }
            }
        }

        if self.decoder.is_none() {
            let video_context = Context::from_parameters(self.parameters.clone()).unwrap();
            let mut decoder = ManagedVideo::new(
                video_context
                    .decoder()
                    .video()
                    .inspect_err(|e| error!("Video decoder failed to start {e}"))
                    .unwrap(),
            );
            self.mode = DecoderMode::Sw;
            let sync = pending_sync.as_ref().map(|pending| pending.mode);
            let mut synced = false;
            for packet in packets {
                if let Err(e) = decoder.send_packet(packet) {
                    error!("send_packet error: {e}");
                }
                while let Some(src_frame) = Self::receive_frame_from(&mut decoder) {
                    let pts = src_frame
                        .pts()
                        .map(|ts| ts.rescale(self.time_base, (1, 1000)));

                    if self.should_skip(pts, sync) {
                        continue;
                    }

                    if let Some(pending_sync) = pending_sync.as_ref()
                        && !synced
                    {
                        let _ = pending_sync.tx.send(Worker::VideoDecoder);
                        synced = true;
                    }

                    let frame = wrap_frame(src_frame, pts, self.mode, &mut self.printed);
                    let _ = self.frame_tx.send(frame);
                }
            }
            if let Some(pending_sync) = pending_sync.as_ref()
                && !synced
            {
                let _ = pending_sync.tx.send(Worker::VideoDecoder);
            }
            self.decoder = Some(decoder);
        }
    }

    fn init_hw(
        &mut self,
        hw_option: &HwOption,
        packets: &mut Vec<TrackedPacket>,
        pending_sync: Option<&PendingSync>,
    ) -> Option<ManagedVideo> {
        let sync = pending_sync.map(|pending| pending.mode);
        let initialized = Arc::new(AtomicBool::new(false));
        let mut decoder = create_decoder(&self.parameters, initialized.clone(), hw_option);
        let mut i = 0;
        let mut synced = false;
        while i <= packets.len() {
            if i == packets.len() {
                let packet = self.packet_tx.recv().unwrap();
                packets.push(packet);
            }
            if let Err(e) = decoder.send_packet(&packets[i]) {
                error!("send_packet error: {e}");
            }
            let mut src_frame = ffmpeg_next::frame::Video::empty();
            match decoder.receive_frame(&mut src_frame) {
                Err(ffmpeg_next::Error::Other {
                    errno: ffmsys::EAGAIN,
                }) => {
                    i += 1;
                }
                Err(e) => {
                    error!(?e);
                    break;
                }
                Ok(()) => {
                    if initialized.load(Ordering::Relaxed) {
                        let pts = src_frame
                            .pts()
                            .map(|ts| ts.rescale(self.time_base, (1, 1000)));

                        let skip = self.should_skip(pts, sync);

                        if !skip {
                            if let Some(pending_sync) = pending_sync
                                && !synced
                            {
                                let _ = pending_sync.tx.send(Worker::VideoDecoder);
                                synced = true;
                            }
                            let frame = wrap_frame(src_frame, pts, self.mode, &mut self.printed);
                            let _ = self.frame_tx.send(frame);
                        }

                        while let Some(src_frame) = Self::receive_frame_from(&mut decoder) {
                            let pts = src_frame
                                .pts()
                                .map(|ts| ts.rescale(self.time_base, (1, 1000)));

                            if self.should_skip(pts, sync) {
                                continue;
                            }

                            if let Some(pending_sync) = pending_sync
                                && !synced
                            {
                                let _ = pending_sync.tx.send(Worker::VideoDecoder);
                                synced = true;
                            }

                            let frame = wrap_frame(src_frame, pts, self.mode, &mut self.printed);
                            let _ = self.frame_tx.send(frame);
                        }

                        for packet in &packets[i + 1..] {
                            if let Err(e) = decoder.send_packet(packet) {
                                error!("send_packet error: {e}");
                            }
                            while let Some(src_frame) = Self::receive_frame_from(&mut decoder) {
                                let pts = src_frame
                                    .pts()
                                    .map(|ts| ts.rescale(self.time_base, (1, 1000)));

                                if self.should_skip(pts, sync) {
                                    continue;
                                }

                                if let Some(pending_sync) = pending_sync
                                    && !synced
                                {
                                    let _ = pending_sync.tx.send(Worker::VideoDecoder);
                                    synced = true;
                                }

                                let frame =
                                    wrap_frame(src_frame, pts, self.mode, &mut self.printed);
                                let _ = self.frame_tx.send(frame);
                            }
                        }

                        if let Some(pending_sync) = pending_sync
                            && !synced
                        {
                            let _ = pending_sync.tx.send(Worker::VideoDecoder);
                        }

                        return Some(decoder);
                    }
                    break;
                }
            }
        }
        None
    }

    fn decode(&mut self, state: &mut DecoderState, packet: &Packet) {
        if let Some(decoder) = &mut self.decoder
            && let Err(e) = decoder.send_packet(packet)
        {
            error!("send_packet error: {e}");
        }

        while let Some(frame) = self.receive_frame() {
            let pts = frame.pts().map(|ts| ts.rescale(self.time_base, (1, 1000)));
            crossbeam_channel::select_biased! {
                recv(self.effect_rx) -> effect => {
                    let effect = effect.unwrap();
                    self.effect_recv(effect, state);
                }
                send(self.frame_tx, wrap_frame(frame, pts, self.mode, &mut self.printed)) -> _res => {},
            }
        }
    }

    fn sync(&mut self, state: &mut DecoderState, packet: &Packet) {
        let (sync_tx, mode) = match state {
            DecoderState::Syncing(sync_tx, mode) => (sync_tx.clone(), *mode),
            _ => return,
        };

        if let Some(decoder) = &mut self.decoder
            && let Err(e) = decoder.send_packet(packet)
        {
            error!("send_packet error: {e}");
        }

        let mut synced = false;
        let mut interrupted = false;
        while let Some(frame) = self.receive_frame() {
            let pts = frame.pts().map(|ts| ts.rescale(self.time_base, (1, 1000)));

            let target = match mode {
                SyncMode::Target(target) => target,
                SyncMode::FollowClock => self.clock.get_ms(),
            };
            if pts.is_some_and(|pts| pts < target) {
                continue;
            }

            crossbeam_channel::select_biased! {
                recv(self.effect_rx) -> effect => {
                    let effect = effect.unwrap();
                    self.effect_recv(effect, state);
                    interrupted = true;
                    break;
                }
                send(self.frame_tx, wrap_frame(frame, pts, self.mode, &mut self.printed)) -> _res => {},
            }
            if !synced {
                let _ = sync_tx.send(Worker::VideoDecoder);
                if let Some(decoder) = &mut self.decoder {
                    decoder.skip_frame(ffmpeg_next::Discard::Default);
                }
                synced = true;
            }
        }
        if synced && !interrupted {
            *state = DecoderState::Active;
        }
    }

    fn receive_frame_from(decoder: &mut ManagedVideo) -> Option<frame::Video> {
        let mut video_frame = frame::Video::empty();
        decoder.receive_frame(&mut video_frame).ok()?;
        Some(video_frame)
    }

    fn receive_frame(&mut self) -> Option<frame::Video> {
        let Some(decoder) = &mut self.decoder else {
            return None;
        };
        let mut audio = frame::Video::empty();
        decoder.receive_frame(&mut audio).ok()?;
        Some(audio)
    }

    fn should_skip(&self, pts: Option<i64>, sync: Option<SyncMode>) -> bool {
        let Some(pts) = pts else {
            return false;
        };
        match sync {
            Some(SyncMode::Target(target)) => pts < target,
            Some(SyncMode::FollowClock) => pts < self.clock.get_ms(),
            None => false,
        }
    }

    fn effect_recv(&mut self, effect: DecoderEffect, state: &mut DecoderState) {
        match effect {
            DecoderEffect::Flush(flush_tx) => {
                while self.packet_tx.try_recv().is_ok() {}
                if let Some(decoder) = &mut self.decoder {
                    decoder.flush();
                }
                *state = DecoderState::Idle;
                let _ = flush_tx.send(Worker::VideoDecoder);
            }
            DecoderEffect::Sync(sync_tx, mode) => {
                if let Some(decoder) = &mut self.decoder {
                    decoder.skip_frame(ffmpeg_next::Discard::NonReference);
                    *state = DecoderState::Syncing(sync_tx, mode);
                } else {
                    *state = DecoderState::Creating {
                        packets: Vec::new(),
                        sync: Some(PendingSync { tx: sync_tx, mode }),
                    };
                }
            }
            DecoderEffect::Drain => {
                *state = DecoderState::Draining;
                tracing::debug!("State changed: {:?}", state);
            }
            DecoderEffect::Reinit(reinit_tx, parameters, time_base) => {
                self.parameters = parameters;
                self.time_base = time_base;
                self.printed = false;
                self.decoder = None;
                *state = DecoderState::Idle;
                let _ = reinit_tx.send((Worker::VideoDecoder, ReinitDetails::None));
            }
        }
    }
}

impl std::fmt::Debug for DecoderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecoderState::Creating { packets, sync } => f
                .debug_struct("Creating")
                .field("packets_count", &packets.len())
                .field("sync", sync)
                .finish(),
            DecoderState::Active => write!(f, "Active"),
            DecoderState::Syncing(sender, target) => write!(f, "Syncing({sender:?}, {target:?})"),
            DecoderState::Draining => write!(f, "Draining"),
            DecoderState::Idle => write!(f, "Idle"),
        }
    }
}
