use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};
use ffmpeg_next::{
    Rational, Rescale,
    codec::{Context, Id},
    decoder::Subtitle,
};
use libass::{Change, Layer};
use tracing::warn;

use crate::{
    session::{DecoderEffect, PlayerEvent, ReinitDetails, SyncMode, Worker},
    utils::{Clock, TrackedPacket},
};

pub struct SubtitleDecoder {
    decoder: Subtitle,
    packet_tx: Receiver<TrackedPacket>,
    track: Arc<Mutex<Option<libass::Track>>>,
    lib: libass::Library,
    effect_rx: Receiver<DecoderEffect>,
    event_tx: Sender<PlayerEvent>,
    clock: Arc<Clock>,
    time_base: Rational,
    sync_mode: Option<SyncMode>,
}

#[derive(Debug)]
pub struct SubtitleFrame {
    pub layers: Vec<Layer>,
    pub change: Change,
}

impl SubtitleDecoder {
    pub fn new(
        context: Context,
        packet_tx: Receiver<TrackedPacket>,
        track: Arc<Mutex<Option<libass::Track>>>,
        lib: libass::Library,
        time_base: Rational,
        effect_rx: Receiver<DecoderEffect>,
        event_tx: Sender<PlayerEvent>,
        clock: Arc<Clock>,
    ) -> Self {
        let decoder = context.decoder().subtitle().unwrap();
        Self {
            decoder,
            packet_tx,
            track,
            lib,
            effect_rx,
            event_tx,
            clock,
            time_base,
            sync_mode: None,
        }
    }
    pub fn process(&mut self) {
        loop {
            let packet = crossbeam_channel::select_biased! {
                recv(self.effect_rx) -> effect => {
                    self.effect_recv(effect.unwrap());
                    continue
                }
                recv(self.packet_tx) -> packet => packet.unwrap()
            };

            let pts = packet.pts().map(|ts| ts.rescale(self.time_base, (1, 1000)));
            let duration = packet.duration().rescale(self.time_base, (1, 1000));

            let mut sub_out = ffmpeg_next::codec::subtitle::Subtitle::new();
            match self.decoder.id() {
                Id::ASS | Id::SSA => {
                    if let Some(data) = packet.data() {
                        if let Some(mode) = self.sync_mode {
                            let target = match mode {
                                SyncMode::Target(target) => target,
                                SyncMode::FollowClock => self.clock.get_ms(),
                            };
                            if let Some(pts_val) = pts
                                && pts_val + duration < target
                            {
                                continue;
                            }

                            if let Some(track) = &mut *self.track.lock().unwrap() {
                                track.process_chunk(data, pts.unwrap_or(0), duration);
                            }
                            self.sync_mode = None;
                        } else if let Some(track) = &mut *self.track.lock().unwrap() {
                            track.process_chunk(data, pts.unwrap_or(0), duration);
                        }
                    }
                }
                Id::SUBRIP | Id::WEBVTT | Id::MOV_TEXT | Id::TEXT => {
                    self.decoder.decode(&packet.packet, &mut sub_out).unwrap();
                    for rect in sub_out.rects() {
                        match rect {
                            ffmpeg_next::subtitle::Rect::None(_) => {
                                warn!("Subs are none");
                            }
                            ffmpeg_next::subtitle::Rect::Bitmap(_bitmap) => {
                                warn!("Bitmap subs are not supported");
                            }
                            ffmpeg_next::subtitle::Rect::Text(_text) => {
                                warn!("Text subs are not supported");
                            }
                            ffmpeg_next::subtitle::Rect::Ass(ass) => {
                                if let Some(mode) = self.sync_mode {
                                    let target = match mode {
                                        SyncMode::Target(target) => target,
                                        SyncMode::FollowClock => self.clock.get_ms(),
                                    };
                                    if let Some(pts_val) = sub_out.pts()
                                        && pts_val + duration < target
                                    {
                                        continue;
                                    }
                                    if let Some(track) = &mut *self.track.lock().unwrap() {
                                        track.process_chunk(
                                            ass.get().as_bytes(),
                                            sub_out.pts().unwrap_or(0),
                                            duration,
                                        );
                                    }
                                    self.sync_mode = None;
                                } else if let Some(track) = &mut *self.track.lock().unwrap() {
                                    track.process_chunk(
                                        ass.get().as_bytes(),
                                        pts.unwrap_or(0),
                                        duration,
                                    );
                                }
                            }
                        }
                    }
                }
                _ => {
                    warn!("Subtitle id not matched {:?}", self.decoder.id());
                }
            }
        }
    }
    fn effect_recv(&mut self, effect: DecoderEffect) {
        match effect {
            DecoderEffect::Flush(flush_tx) => {
                while self.packet_tx.try_recv().is_ok() {}
                self.decoder.flush();
                self.sync_mode = None;
                if let Some(track) = &mut *self.track.lock().unwrap() {
                    track.flush_events();
                }
                let _ = flush_tx.send(Worker::SubDecoder);
            }
            DecoderEffect::Sync(sync_tx, mode) => {
                self.sync_mode = Some(mode);
                let _ = sync_tx.send(Worker::SubDecoder);
            }
            DecoderEffect::Reinit(reinit_tx, parameters, time_base) => {
                let context = Context::from_parameters(parameters).unwrap();
                let ptr = unsafe { context.as_ptr() };
                if let Ok(decoder) = context.decoder().subtitle() {
                    self.decoder = decoder;
                }
                self.time_base = time_base;
                let mut track = self.lib.new_track().unwrap();
                unsafe {
                    if !(*ptr).extradata.is_null() && (*ptr).extradata_size > 0 {
                        let data = std::slice::from_raw_parts(
                            (*ptr).extradata,
                            (*ptr).extradata_size as usize,
                        );
                        track.process_codec_private(data);
                    } else {
                        let header = "\
                            [Events]\n\
                            Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";
                        track.process_codec_private(header.as_bytes());
                    }
                }
                *self.track.lock().unwrap() = Some(track);
                let _ = reinit_tx.send((Worker::SubDecoder, ReinitDetails::None));
            }
            // TODO work on these when we dont use libass for everything for now it doesnt hurt having these do nothing
            DecoderEffect::Drain => {}
        }
    }
}
