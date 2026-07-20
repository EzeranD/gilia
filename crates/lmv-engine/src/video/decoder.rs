use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError, select_biased};
use ffmpeg_next::{
    Packet, Rational, Rescale,
    codec::{Context, Parameters},
    ffi as ffmsys,
};
use tracing::{debug, error};

pub use crate::video::frame::{DecoderMode, VideoFrame, wrap_frame};
use crate::{
    PlayerEvent::Internal,
    engine::EngineConfig,
    session::{
        DecoderEffect,
        InternalEvent::{VideoDrained, VideoFlushed, VideoSynced},
        PlayerEvent,
    },
    video::hw_ffmpeg::{HwOption, ManagedVideo, create_decoder, get_hw_options},
};

pub struct VideoDecoder {
    ctx: VideoContext,
    state: VideoState,
}

pub struct VideoContext {
    config: Arc<EngineConfig>,
    decoder: Option<ManagedVideo>,
    mode: DecoderMode,
    parameters: Parameters,
    stream: Receiver<Packet>,
    frame_tx: Sender<VideoFrame>,
    effect_rx: Receiver<DecoderEffect>,
    event_tx: Sender<PlayerEvent>,
    time_base: Rational,
    sync_target: Option<i64>,
    printed: bool,
}

pub enum VideoState {
    Creating { packets: Vec<Packet> },
    Active,
    Draining,
    Idle,
}

impl std::fmt::Debug for VideoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VideoState::Creating { packets } => f
                .debug_struct("Creating")
                .field("packets_count", &packets.len())
                .finish(),
            VideoState::Active => write!(f, "Active"),
            VideoState::Draining => write!(f, "Draining"),
            VideoState::Idle => write!(f, "Idle"),
        }
    }
}

impl VideoDecoder {
    #[must_use]
    pub fn new(
        config: Arc<EngineConfig>,
        parameters: Parameters,
        stream: Receiver<Packet>,
        frame_tx: Sender<VideoFrame>,
        time_base: Rational,
        effect_rx: Receiver<DecoderEffect>,
        event_tx: Sender<PlayerEvent>,
    ) -> Self {
        let ctx = VideoContext {
            config,
            decoder: None,
            mode: DecoderMode::Hw,
            parameters,
            stream,
            frame_tx,
            time_base,
            effect_rx,
            event_tx,
            sync_target: None,
            printed: false,
        };
        Self {
            ctx,
            state: VideoState::Creating {
                packets: Vec::new(),
            },
        }
    }

    pub fn process(&mut self) {
        let VideoDecoder { ctx, state } = self;
        loop {
            match state {
                VideoState::Creating { packets } => {
                    ctx.init_decoder(packets);
                    *state = VideoState::Active;
                    debug!("State changed: {:?}", state);
                }
                VideoState::Active => {
                    select_biased! {
                        recv(ctx.effect_rx) -> effect => {
                            let effect = effect.unwrap();
                            ctx.effect_recv(effect, state);
                        }
                        recv(ctx.stream) -> packet => ctx.decode(&packet.unwrap(), state)
                    }
                }
                VideoState::Draining => {
                    match ctx.stream.try_recv() {
                        Ok(packet) => ctx.decode(&packet, state),
                        Err(TryRecvError::Empty) => {
                            let _ = ctx.event_tx.send(Internal(VideoDrained));
                            *state = VideoState::Idle;
                            debug!("State changed: {:?}", state);
                        }
                        Err(TryRecvError::Disconnected) => {} // TODO need to handle this better
                    }
                }
                VideoState::Idle => {
                    let effect = ctx.effect_rx.recv().unwrap();
                    ctx.effect_recv(effect, state);
                }
            }
        }
    }
}

impl VideoContext {
    fn init_decoder(&mut self, packets: &mut Vec<Packet>) {
        if self.config.hw_dec {
            let parameters = self.parameters.clone();
            let mut hw_options = get_hw_options(&parameters);
            for hw_option in &mut hw_options {
                debug!("hw_cfg: {:?}", hw_option.hw_cfg);
                if let Some(decoder) = self.init_hw(hw_option, packets) {
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
            for packet in packets {
                if let Err(e) = decoder.send_packet(packet) {
                    error!("send_packet error: {e}");
                }
                loop {
                    let mut src_frame = ffmpeg_next::frame::Video::empty();
                    if decoder.receive_frame(&mut src_frame).is_err() {
                        break;
                    }
                    self.process_frame(&mut decoder, src_frame);
                }
            }

            self.decoder = Some(decoder);
        }
    }

    fn init_hw(&mut self, hw_option: &HwOption, packets: &mut Vec<Packet>) -> Option<ManagedVideo> {
        let initialized = Arc::new(AtomicBool::new(false));
        let mut decoder = create_decoder(&self.parameters, initialized.clone(), hw_option);
        let mut i = 0;
        while i <= packets.len() {
            if i == packets.len() {
                let packet = self.stream.recv().unwrap();
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
                        self.process_frame(&mut decoder, src_frame);

                        for packet in &packets[i + 1..] {
                            if let Err(e) = decoder.send_packet(packet) {
                                error!("send_packet error: {e}");
                            }
                            loop {
                                let mut src_frame = ffmpeg_next::frame::Video::empty();
                                if decoder.receive_frame(&mut src_frame).is_err() {
                                    break;
                                }
                                self.process_frame(&mut decoder, src_frame);
                            }
                        }

                        return Some(decoder);
                    }
                    break;
                }
            }
        }
        None
    }

    fn decode(&mut self, packet: &Packet, state: &mut VideoState) {
        if let Some(decoder) = &mut self.decoder
            && let Err(e) = decoder.send_packet(packet)
        {
            error!("send_packet error: {e}");
        }

        loop {
            let mut src_frame = ffmpeg_next::frame::Video::empty();
            if let Some(decoder) = &mut self.decoder
                && let Err(_) = decoder.receive_frame(&mut src_frame)
            {
                break;
            }

            let pts = src_frame
                .pts()
                .map(|ts| ts.rescale(self.time_base, (1, 1000)));

            if let Some(sync_pts) = self.sync_target {
                if pts.unwrap() >= sync_pts {
                    crossbeam_channel::select_biased! {
                        recv(self.effect_rx) -> effect => {
                            let effect = effect.unwrap();
                            self.effect_recv(effect, state);
                            continue
                        }
                        send(self.frame_tx, wrap_frame(src_frame, pts, self.mode, &mut self.printed)) -> _res => {},
                    }
                    self.sync_target = None;
                    if let Some(decoder) = &mut self.decoder {
                        decoder.skip_frame(ffmpeg_next::Discard::Default);
                    }
                    let _ = self.event_tx.send(Internal(VideoSynced));
                }
            } else {
                crossbeam_channel::select_biased! {
                    recv(self.effect_rx) -> effect => {
                        let effect = effect.unwrap();
                        self.effect_recv(effect, state);
                    }
                    send(self.frame_tx, wrap_frame(src_frame, pts, self.mode, &mut self.printed)) -> _res => {},
                }
            }
        }
    }

    fn process_frame(&mut self, decoder: &mut ManagedVideo, src_frame: ffmpeg_next::frame::Video) {
        let pts = src_frame
            .pts()
            .map(|ts| ts.rescale(self.time_base, (1, 1000)));

        if let Some(sync_pts) = self.sync_target {
            if pts.unwrap() >= sync_pts {
                let frame = wrap_frame(src_frame, pts, self.mode, &mut self.printed);
                let _ = self.frame_tx.send(frame);

                self.sync_target = None;
                decoder.skip_frame(ffmpeg_next::Discard::Default);
                let _ = self.event_tx.send(Internal(VideoSynced));
            }
        } else {
            let frame = wrap_frame(src_frame, pts, self.mode, &mut self.printed);
            let _ = self.frame_tx.send(frame);
        }
    }

    fn effect_recv(&mut self, effect: DecoderEffect, state: &mut VideoState) {
        match effect {
            DecoderEffect::Flush => {
                while self.stream.try_recv().is_ok() {}
                if let Some(decoder) = &mut self.decoder {
                    decoder.flush();
                }
                *state = VideoState::Active;
                let _ = self.event_tx.send(Internal(VideoFlushed));
            }
            DecoderEffect::Sync(pts) => {
                self.sync_target = Some(pts);
                if let Some(decoder) = &mut self.decoder {
                    decoder.skip_frame(ffmpeg_next::Discard::NonReference);
                }
                *state = VideoState::Active;
            }
            DecoderEffect::Drain => {
                *state = VideoState::Draining;
                tracing::debug!("State changed: {:?}", state);
            }
        }
    }
}
