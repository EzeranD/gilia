use std::{mem, thread};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use ffmpeg_next::{
    ChannelLayout, Packet, Rational, Rescale,
    decoder::Audio,
    format::{self, sample},
    frame,
    software::{resampler, resampling},
};
use tracing::{debug, error};

use crate::{
    audio::queue::{AudioBlock, Producer, PushError},
    core::{DecoderEffect, PlayerEvent},
};

pub struct AudioDecoder {
    decoder: Audio,
    resampler: resampling::Context,
    stream: Receiver<Packet>,
    audio_buffer: Producer,
    time_base: Rational,
    effect_rx: Receiver<DecoderEffect>,
    event_tx: Sender<PlayerEvent>,
    sync_target: Option<i64>,
    state: DecoderState,
}

#[derive(Debug)]
pub enum DecoderState {
    Active,
    Draining,
    Idle,
}

impl AudioDecoder {
    pub fn new(
        decoder: Audio,
        stream: Receiver<Packet>,
        audio_buffer: Producer,
        time_base: Rational,
        effect_rx: Receiver<DecoderEffect>,
        event_tx: Sender<PlayerEvent>,
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
            stream,
            audio_buffer,
            time_base,
            effect_rx,
            event_tx,
            sync_target: None,
            state: DecoderState::Active,
        }
    }

    pub fn process(&mut self) {
        loop {
            match &mut self.state {
                DecoderState::Active => {
                    crossbeam_channel::select_biased! {
                        recv(self.effect_rx) -> effect => {
                            self.effect_recv(effect.unwrap());
                        }
                        recv(self.stream) -> packet => {
                            self.decode(&packet.unwrap());
                        }
                    }
                }
                DecoderState::Draining => {
                    match self.stream.try_recv() {
                        Ok(packet) => self.decode(&packet),
                        Err(TryRecvError::Empty) => {
                            let _ = self.event_tx.send(PlayerEvent::AudioDrained);
                            self.state = DecoderState::Idle;
                            debug!("State changed: {:?}", self.state);
                        }
                        Err(TryRecvError::Disconnected) => {} // TODO need to handle this better
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

        'receive: loop {
            let mut audio = frame::Audio::empty();
            if self.decoder.receive_frame(&mut audio).is_err() {
                break;
            }

            let mut new_audio = frame::Audio::empty();
            let _ = self.resampler.run(&audio, &mut new_audio);

            let plane = new_audio.plane::<(f32, f32)>(0);
            let samples: Vec<f32> = plane.iter().flat_map(|&(l, r)| [l, r]).collect();
            let bytes: &[u8] = bytemuck::cast_slice(&samples);
            let frames = bytes.len() / (2 * mem::size_of::<f32>());
            let block_pts = packet_pts;
            let block_duration = new_audio.samples() as u32 * 1000 / self.decoder.rate();
            packet_pts += block_duration as i64;

            let mut block = AudioBlock {
                data: Box::from(bytes),
                frames,
                pts: block_pts,
            };

            if let Some(sync_pts) = self.sync_target {
                if block_pts < sync_pts {
                    continue;
                }
                loop {
                    match self.effect_rx.try_recv() {
                        Ok(effect) => {
                            self.effect_recv(effect);
                            break 'receive;
                        }
                        Err(TryRecvError::Disconnected) => return,
                        Err(TryRecvError::Empty) => {}
                    }
                    match self.audio_buffer.try_push(block) {
                        Ok(()) => break,
                        Err(PushError::Full(ret)) => block = ret,
                    }
                    thread::park();
                }
                self.sync_target = None;
                let _ = self.event_tx.send(PlayerEvent::AudioSynced);
            } else {
                loop {
                    match self.effect_rx.try_recv() {
                        Ok(effect) => {
                            self.effect_recv(effect);
                            break 'receive;
                        }
                        Err(TryRecvError::Disconnected) => return,
                        Err(TryRecvError::Empty) => {}
                    }
                    match self.audio_buffer.try_push(block) {
                        Ok(()) => {
                            break;
                        }
                        Err(PushError::Full(ret)) => block = ret,
                    }
                    thread::park();
                }
            }
        }
    }

    fn effect_recv(&mut self, effect: DecoderEffect) {
        match effect {
            DecoderEffect::Flush => {
                while self.stream.try_recv().is_ok() {}
                self.decoder.flush();
                self.state = DecoderState::Active;
                let _ = self.event_tx.send(PlayerEvent::AudioFlushed);
            }
            DecoderEffect::Sync(pts) => {
                self.sync_target = Some(pts);
                self.state = DecoderState::Active;
            }
            DecoderEffect::Drain => {
                self.state = DecoderState::Draining;
                debug!("State changed: {:?}", self.state);
            }
        }
    }
}
