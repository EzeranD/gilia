use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};
use ffmpeg_next::{
    Packet, Rational, Rescale,
    codec::{Context, Id},
    decoder::Subtitle,
};
use libass::{Change, Layer};
use tracing::warn;

use crate::core::{DecoderEffect, PlayerEvent};

pub struct SubtitleDecoder {
    decoder: Subtitle,
    stream: Receiver<Packet>,
    track: Arc<Mutex<libass::Track>>,
    effect_rx: Receiver<DecoderEffect>,
    event_tx: Sender<PlayerEvent>,
    time_base: Rational,
}

#[derive(Debug)]
pub struct SubtitleFrame {
    pub layers: Vec<Layer>,
    pub change: Change,
}

impl SubtitleDecoder {
    pub fn new(
        context: Context,
        stream: Receiver<Packet>,
        track: Arc<Mutex<libass::Track>>,
        time_base: Rational,
        effect_rx: Receiver<DecoderEffect>,
        event_tx: Sender<PlayerEvent>,
    ) -> Self {
        let decoder = context.decoder().subtitle().unwrap();
        Self {
            decoder,
            stream,
            track,
            effect_rx,
            event_tx,
            time_base,
        }
    }
    pub fn process(&mut self) {
        loop {
            let packet = crossbeam_channel::select_biased! {
                recv(self.effect_rx) -> effect => {
                    self.effect_recv(effect.unwrap());
                    continue
                }
                recv(self.stream) -> packet => packet.unwrap()
            };

            let pts = packet.pts().map(|ts| ts.rescale(self.time_base, (1, 1000)));
            let duration = packet.duration().rescale(self.time_base, (1, 1000));

            let mut sub_out = ffmpeg_next::codec::subtitle::Subtitle::new();
            match self.decoder.id() {
                Id::ASS | Id::SSA => {
                    if let Some(data) = packet.data() {
                        let mut track = self.track.lock().unwrap();
                        track.process_chunk(data, pts.unwrap(), duration);
                    }
                }
                Id::SUBRIP | Id::WEBVTT | Id::MOV_TEXT | Id::TEXT => {
                    self.decoder.decode(&packet, &mut sub_out).unwrap();
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
                                let mut track = self.track.lock().unwrap();
                                track.process_chunk(ass.get().as_bytes(), pts.unwrap(), duration);
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
            DecoderEffect::Flush => {
                while self.stream.try_recv().is_ok() {}
                self.decoder.flush();
                self.track.lock().unwrap().flush_events();
            }
            // TODO work on these when we dont use libass for everything for now it doesnt hurt having these do nothing
            DecoderEffect::Sync(_) | DecoderEffect::Drain => {}
        }
    }
}
