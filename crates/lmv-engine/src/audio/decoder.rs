use std::{mem, sync::Arc, thread};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use ffmpeg_next::{
    ChannelLayout, Packet, Rational, Rescale,
    codec::Context,
    decoder::Audio,
    format::{self, sample},
    frame,
    software::{resampler, resampling},
};
use tracing::{debug, error};

use crate::{
    PlayerEvent::Internal,
    audio::queue::{AudioBlock, Producer, PushError},
    session::{
        DecoderEffect,
        InternalEvent::{Drained, Flushed, Reinitialized, Synced},
        PlayerEvent, ReinitDetails, SyncMode, Worker,
    },
    utils::{Clock, TrackedPacket},
};

pub struct AudioDecoder {
    decoder: Audio,
    resampler: resampling::Context,
    packet_tx: Receiver<TrackedPacket>,
    audio_buffer: Producer,
    time_base: Rational,
    effect_rx: Receiver<DecoderEffect>,
    event_tx: Sender<PlayerEvent>,
    clock: Arc<Clock>,
    state: DecoderState,
}

#[derive(Debug, Clone, Copy)]
pub enum DecoderState {
    Active,
    Syncing(SyncMode),
    Draining,
    Idle,
}

enum PushBlockResult {
    Pushed,
    Interrupted,
    Disconnected,
}

impl AudioDecoder {
    pub fn new(
        decoder: Audio,
        packet_tx: Receiver<TrackedPacket>,
        audio_buffer: Producer,
        time_base: Rational,
        effect_rx: Receiver<DecoderEffect>,
        event_tx: Sender<PlayerEvent>,
        clock: Arc<Clock>,
    ) -> Self {
        let resampler = resampler(
            (decoder.format(), decoder.channel_layout(), decoder.rate()),
            (
                format::Sample::F32(sample::Type::Packed),
                ChannelLayout::STEREO,
                decoder.rate(),
            ),
        )
        .unwrap();

        Self {
            decoder,
            resampler,
            packet_tx,
            audio_buffer,
            time_base,
            effect_rx,
            event_tx,
            clock,
            state: DecoderState::Active,
        }
    }

    pub fn process(&mut self) {
        loop {
            match self.state {
                DecoderState::Active => {
                    crossbeam_channel::select_biased! {
                        recv(self.effect_rx) -> effect => {
                            self.effect_recv(effect.unwrap());
                        }
                        recv(self.packet_tx) -> packet => {
                            self.decode(&packet.unwrap());
                        }
                    }
                }
                DecoderState::Syncing(mode) => {
                    crossbeam_channel::select_biased! {
                        recv(self.effect_rx) -> effect => {
                            self.effect_recv(effect.unwrap());
                        }
                        recv(self.packet_tx) -> packet => {
                            self.sync(&packet.unwrap(), mode);
                        }
                    }
                }
                DecoderState::Draining => {
                    match self.effect_rx.try_recv() {
                        Ok(effect) => {
                            self.effect_recv(effect);
                            continue;
                        }
                        Err(TryRecvError::Disconnected) => return,
                        Err(TryRecvError::Empty) => {}
                    }

                    match self.packet_tx.try_recv() {
                        Ok(packet) => self.decode(&packet),
                        Err(TryRecvError::Empty) => {
                            let _ = self.event_tx.send(Internal(Drained(Worker::AudioDecoder)));
                            self.state = DecoderState::Idle;
                            debug!("State changed: {:?}", self.state);
                        }
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                DecoderState::Idle => {
                    let effect = self.effect_rx.recv().unwrap();
                    self.effect_recv(effect);
                }
            }
        }
    }

    fn decode(&mut self, packet: &Packet) {
        let mut packet_pts = packet
            .pts()
            .map(|ts| ts.rescale(self.time_base, (1, 1000)))
            .unwrap();

        if let Err(e) = self.decoder.send_packet(packet) {
            error!("send_packet error: {e}");
        }
        let rate = self.decoder.rate();
        while let Some(audio) = self.receive_frame() {
            let mut new_audio = frame::Audio::empty();
            let _ = self.resampler.run(&audio, &mut new_audio);

            let plane = new_audio.plane::<(f32, f32)>(0);
            let samples: Vec<f32> = plane.iter().flat_map(|&(l, r)| [l, r]).collect();
            let bytes: &[u8] = bytemuck::cast_slice(&samples);
            let frames = bytes.len() / (2 * mem::size_of::<f32>());
            let block_pts = packet_pts;
            let block_duration = new_audio.samples() as u32 * 1000 / rate;
            packet_pts += block_duration as i64;

            let block = AudioBlock {
                data: Box::from(bytes),
                frames,
                pts: block_pts,
            };

            match self.push_block(block) {
                PushBlockResult::Pushed => {}
                PushBlockResult::Interrupted => break,
                PushBlockResult::Disconnected => return,
            }
        }
    }

    fn sync(&mut self, packet: &Packet, mode: SyncMode) {
        let mut packet_pts = packet
            .pts()
            .map(|ts| ts.rescale(self.time_base, (1, 1000)))
            .unwrap();

        if let Err(e) = self.decoder.send_packet(packet) {
            error!("send_packet error: {e}");
        }
        let mut synced = false;
        let mut interrupted = false;
        let rate = self.decoder.rate();
        while let Some(frame) = self.receive_frame() {
            let block_pts = packet_pts;
            let block_duration = frame.samples() as u32 * 1000 / rate;
            packet_pts += block_duration as i64;
            let target = match mode {
                SyncMode::Target(target) => target,
                SyncMode::FollowClock => self.clock.get_ms(),
            };
            if block_pts < target {
                continue;
            }
            let mut new_audio = frame::Audio::empty();
            let _ = self.resampler.run(&frame, &mut new_audio);

            let plane = new_audio.plane::<(f32, f32)>(0);
            let samples: Vec<f32> = plane.iter().flat_map(|&(l, r)| [l, r]).collect();
            let bytes: &[u8] = bytemuck::cast_slice(&samples);
            let frames = bytes.len() / (2 * mem::size_of::<f32>());
            let block = AudioBlock {
                data: Box::from(bytes),
                frames,
                pts: block_pts,
            };
            match self.push_block(block) {
                PushBlockResult::Pushed => {}
                PushBlockResult::Interrupted => {
                    interrupted = true;
                    break;
                }
                PushBlockResult::Disconnected => return,
            }
            if !synced {
                let _ = self.event_tx.send(Internal(Synced(Worker::AudioDecoder)));
                synced = true;
            }
        }
        if synced && !interrupted {
            self.state = DecoderState::Active;
        }
    }

    fn receive_frame(&mut self) -> Option<frame::Audio> {
        let mut audio = frame::Audio::empty();
        self.decoder.receive_frame(&mut audio).ok()?;
        Some(audio)
    }

    fn push_block(&mut self, mut block: AudioBlock) -> PushBlockResult {
        loop {
            match self.effect_rx.try_recv() {
                Ok(effect) => {
                    self.effect_recv(effect);
                    return PushBlockResult::Interrupted;
                }
                Err(TryRecvError::Disconnected) => return PushBlockResult::Disconnected,
                Err(TryRecvError::Empty) => {}
            }
            match self.audio_buffer.try_push(block) {
                Ok(()) => return PushBlockResult::Pushed,
                Err(PushError::Full(ret)) => block = ret,
            }
            thread::park();
        }
    }

    fn effect_recv(&mut self, effect: DecoderEffect) {
        match effect {
            DecoderEffect::Flush => {
                while self.packet_tx.try_recv().is_ok() {}
                self.decoder.flush();
                self.state = DecoderState::Idle;
                let _ = self.event_tx.send(Internal(Flushed(Worker::AudioDecoder)));
            }
            DecoderEffect::Sync(mode) => {
                self.state = DecoderState::Syncing(mode);
            }
            DecoderEffect::Drain => {
                self.state = DecoderState::Draining;
                debug!("State changed: {:?}", self.state);
            }
            DecoderEffect::Reinit(params, time_base) => {
                let audio_context = Context::from_parameters(params).unwrap();
                self.decoder = audio_context.decoder().audio().unwrap();
                self.time_base = time_base;
                self.resampler = resampler(
                    (
                        self.decoder.format(),
                        self.decoder.channel_layout(),
                        self.decoder.rate(),
                    ),
                    (
                        format::Sample::F32(sample::Type::Packed),
                        ChannelLayout::STEREO,
                        self.decoder.rate(),
                    ),
                )
                .unwrap();
                let _ = self.event_tx.send(Internal(Reinitialized {
                    worker: Worker::AudioDecoder,
                    details: ReinitDetails::Audio {
                        rate: self.decoder.rate(),
                    },
                }));
            }
        }
    }
}
