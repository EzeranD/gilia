use crossbeam_channel::{Receiver, Sender};
use ffmpeg_next::{Packet, Rescale, format::context::Input};

use crate::core::{
    DemuxerEffect,
    PlayerEvent::{self, DemuxerEof},
};

pub struct Demuxer {
    ictx: Input,
    video_idx: Option<usize>,
    audio_idx: Option<usize>,
    sub_idx: Option<usize>,
    video_stream_tx: Option<Sender<Packet>>,
    audio_stream_tx: Option<Sender<Packet>>,
    sub_stream_tx: Option<Sender<Packet>>,
    effect_rx: Receiver<DemuxerEffect>,
    event_tx: Sender<PlayerEvent>,
    video_last_pts: i64,
    audio_last_pts: i64,
    sub_last_pts: i64,
}

impl Demuxer {
    pub fn new(
        ictx: Input,
        video_idx: Option<usize>,
        audio_idx: Option<usize>,
        sub_idx: Option<usize>,
        video_stream_tx: Option<Sender<Packet>>,
        audio_stream_tx: Option<Sender<Packet>>,
        sub_stream_tx: Option<Sender<Packet>>,
        effect_rx: Receiver<DemuxerEffect>,
        event_tx: Sender<PlayerEvent>,
    ) -> Self {
        Self {
            ictx,
            video_idx,
            audio_idx,
            sub_idx,
            video_stream_tx,
            audio_stream_tx,
            sub_stream_tx,
            effect_rx,
            event_tx,
            video_last_pts: 0,
            audio_last_pts: 0,
            sub_last_pts: 0,
        }
    }

    pub fn read_packets(&mut self) {
        loop {
            let mut received_effect = None;
            loop {
                let mut packet = Packet::empty();
                match packet.read(&mut self.ictx) {
                    Ok(()) => {
                        let idx = packet.stream();
                        let time_base = self.ictx.stream(idx).unwrap().time_base();
                        let pts = packet.pts().map(|ts| ts.rescale(time_base, (1, 1000)));
                        if Some(idx) == self.video_idx {
                            let Some(video_tx) = &self.video_stream_tx else {
                                continue;
                            };
                            crossbeam_channel::select_biased! {
                                recv(self.effect_rx) -> effect => {
                                    received_effect = effect.ok();
                                    break
                                }
                                send(video_tx, packet) -> _res => {
                                    self.video_last_pts = pts.unwrap_or(0);
                                }
                            }
                        } else if Some(idx) == self.audio_idx {
                            let Some(audio_tx) = &self.audio_stream_tx else {
                                continue;
                            };
                            crossbeam_channel::select_biased! {
                                recv(self.effect_rx) -> effect => {
                                    received_effect = effect.ok();
                                    break
                                }
                                send(audio_tx, packet) -> _res => {
                                    self.audio_last_pts = pts.unwrap_or(0);
                                }
                            }
                        } else if Some(idx) == self.sub_idx {
                            let Some(sub_tx) = &self.sub_stream_tx else {
                                continue;
                            };
                            crossbeam_channel::select_biased! {
                                recv(self.effect_rx) -> effect => {
                                    received_effect = effect.ok();
                                    break
                                }
                                send(sub_tx, packet) -> _res => {
                                    self.sub_last_pts = pts.unwrap_or(0);
                                }
                            }
                        }
                    }
                    Err(ffmpeg_next::Error::Eof) => {
                        let _ = self.event_tx.send(DemuxerEof);
                        if let Ok(effect) = self.effect_rx.recv() {
                            self.effect_recv(effect);
                        }
                        break;
                    }
                    Err(_) => {}
                }
            }
            if let Some(effect) = received_effect {
                self.effect_recv(effect);
            }
        }
    }
    fn effect_recv(&mut self, effect: DemuxerEffect) {
        if let DemuxerEffect::SeekDemuxer(pts) = effect {
            let pts_us = pts * 1000;
            let _ = self.ictx.seek(pts_us, i64::MIN..pts_us);
            let _ = self.event_tx.send(PlayerEvent::DemuxerSeeked);
            if let Ok(DemuxerEffect::ResumeDemuxer) = self.effect_rx.recv() {}
        }
    }
}
