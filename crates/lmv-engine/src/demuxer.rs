use std::{sync::Arc, thread, time::Duration};

use crossbeam_channel::{Receiver, Sender};
use ffmpeg_next::{Packet, Rescale, format::context::Input, media::Type};

use crate::{
    PlayerEvent::{self, Internal},
    TrackKind,
    session::{DemuxerEffect, InternalEvent::DemuxerEof},
    utils::{Clock, MemoryBudget, ThreadWaker, TrackedPacket},
};

pub struct Demuxer {
    ictx: Input,
    video_stream: StreamInfo,
    audio_stream: StreamInfo,
    sub_stream: StreamInfo,
    effect_rx: Receiver<DemuxerEffect>,
    event_tx: Sender<PlayerEvent>,
    clock: Arc<Clock>,
    pub waker: Arc<ThreadWaker>,
    budget: MemoryBudget,
}

struct StreamInfo {
    idx: Option<usize>,
    stream_type: Type,
    packet_tx: Option<Sender<TrackedPacket>>,
    target_dts: Option<i64>,
    last_dts: Option<i64>,
}

impl Demuxer {
    pub fn new(
        ictx: Input,
        video_idx: Option<usize>,
        audio_idx: Option<usize>,
        sub_idx: Option<usize>,
        video_stream_tx: Option<Sender<TrackedPacket>>,
        audio_stream_tx: Option<Sender<TrackedPacket>>,
        sub_stream_tx: Option<Sender<TrackedPacket>>,
        effect_rx: Receiver<DemuxerEffect>,
        event_tx: Sender<PlayerEvent>,
        clock: Arc<Clock>,
        waker: Arc<ThreadWaker>,
        budget: MemoryBudget,
    ) -> Self {
        let video_stream = StreamInfo {
            idx: video_idx,
            stream_type: Type::Video,
            packet_tx: video_stream_tx,
            target_dts: None,
            last_dts: None,
        };
        let audio_stream = StreamInfo {
            idx: audio_idx,
            stream_type: Type::Audio,
            packet_tx: audio_stream_tx,
            target_dts: None,
            last_dts: None,
        };
        let sub_stream = StreamInfo {
            idx: sub_idx,
            stream_type: Type::Subtitle,
            packet_tx: sub_stream_tx,
            target_dts: None,
            last_dts: None,
        };
        Self {
            ictx,
            video_stream,
            audio_stream,
            sub_stream,
            effect_rx,
            event_tx,
            clock,
            waker,
            budget,
        }
    }

    pub fn read_packets(&mut self) {
        loop {
            let mut received_effect = None;
            loop {
                if let Ok(effect) = self.effect_rx.try_recv() {
                    received_effect = Some(effect);
                    break;
                }
                if self.budget.over_limit() {
                    thread::park_timeout(Duration::from_millis(10));
                    continue;
                }
                let mut packet = Packet::empty();
                match packet.read(&mut self.ictx) {
                    Ok(()) => {
                        let idx = packet.stream();
                        let time_base = self.ictx.stream(idx).unwrap().time_base();
                        let pts = packet.pts().map(|ts| ts.rescale(time_base, (1, 1000)));
                        let dts = packet.dts().map(|ts| ts.rescale(time_base, (1, 1000)));
                        let streams = [
                            &mut self.video_stream,
                            &mut self.audio_stream,
                            &mut self.sub_stream,
                        ];

                        let Some(stream) = streams.into_iter().find(|s| s.idx == Some(idx)) else {
                            continue;
                        };

                        let Some(packet_tx) = &stream.packet_tx else {
                            continue;
                        };

                        if let Some(target_dts) = stream.target_dts
                            && dts.unwrap_or(0) <= target_dts
                        {
                            continue;
                        }

                        let tracked_packet = TrackedPacket::new(packet, self.budget.clone());

                        crossbeam_channel::select_biased! {
                            recv(self.effect_rx) -> effect => {
                                received_effect = effect.ok();
                                break;
                            }
                            send(packet_tx, tracked_packet) -> _res => {
                                stream.last_dts = dts;
                                stream.target_dts = None;
                            }
                        }
                    }
                    Err(ffmpeg_next::Error::Eof) => {
                        let _ = self.event_tx.send(Internal(DemuxerEof));
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
        match effect {
            DemuxerEffect::ResumeDemuxer => {}
            DemuxerEffect::SeekDemuxer(seek_tx, pts) => {
                self.video_stream.target_dts = None;
                self.audio_stream.target_dts = None;
                self.sub_stream.target_dts = None;
                self.video_stream.last_dts = None;
                self.audio_stream.last_dts = None;
                self.sub_stream.last_dts = None;
                let pts_us = pts * 1000;
                let _ = self.ictx.seek(pts_us, i64::MIN..pts_us);
                let _ = seek_tx.send(());
                if let Ok(DemuxerEffect::ResumeDemuxer) = self.effect_rx.recv() {}
            }
            DemuxerEffect::SwitchStream { seek_tx, kind, id } => {
                let clock_ms = self.clock.get_ms();
                let pts_us = match kind {
                    TrackKind::Audio => {
                        self.audio_stream.idx = Some(id);
                        self.audio_stream.target_dts = None;
                        self.video_stream.target_dts = self.video_stream.last_dts;
                        self.sub_stream.target_dts = self.sub_stream.last_dts;
                        clock_ms * 1000
                    }
                    TrackKind::Video => {
                        self.video_stream.idx = Some(id);
                        self.video_stream.target_dts = None;
                        self.audio_stream.target_dts = self.audio_stream.last_dts;
                        self.sub_stream.target_dts = self.sub_stream.last_dts;
                        clock_ms * 1000
                    }
                    TrackKind::Subtitle => {
                        self.sub_stream.idx = Some(id);
                        self.sub_stream.target_dts = None;
                        self.audio_stream.target_dts = self.audio_stream.last_dts;
                        self.video_stream.target_dts = self.video_stream.last_dts;
                        clock_ms.saturating_sub(10000) * 1000
                    }
                };
                let _ = self.ictx.seek(pts_us, i64::MIN..pts_us);

                let _ = seek_tx.send(());
                if let Ok(DemuxerEffect::ResumeDemuxer) = self.effect_rx.recv() {}
            }
        }
    }
}
