use std::{
    sync::{Arc, Mutex, atomic::AtomicI64},
    thread::JoinHandle,
};

use crossbeam_channel::Sender;
use ffmpeg_next::{Packet, Stream};

use crate::{
    EngineConfig, PlayerEvent, VideoFrame,
    engine::ExternalCallback,
    session::{DecoderEffect, VoEffect},
    utils::{Clock, ThreadWaker, WakingSender},
    video::{decoder::VideoDecoder, output::VideoOutput},
};

pub mod decoder;
pub mod frame;
mod hw_ffmpeg;
pub mod output;

pub fn spawn_video_stream(
    video_stream: &(Stream, usize),
    event_tx: &Sender<PlayerEvent>,
    external_callback: Option<&ExternalCallback>,
    config: &Arc<EngineConfig>,
    frame: &Arc<Mutex<Option<VideoFrame>>>,
    current_pts: &Arc<AtomicI64>,
    audio_clock: &Arc<Clock>,
) -> (
    JoinHandle<()>,
    JoinHandle<()>,
    Sender<DecoderEffect>,
    WakingSender<VoEffect>,
    Sender<Packet>,
) {
    let (frame_tx, frame_rx) = crossbeam_channel::bounded(3);
    let (video_tx, video_rx) = crossbeam_channel::unbounded();
    let (video_packet_tx, video_packet_rx) = crossbeam_channel::bounded(3);

    let video_time_base = video_stream.0.time_base();
    let parameters = video_stream.0.parameters();
    let video_event_tx = event_tx.clone();
    let config = config.clone();
    let dec_handle = std::thread::Builder::new()
        .name("video-decoder".into())
        .spawn(move || {
            let mut decoder = VideoDecoder::new(
                config,
                parameters,
                video_packet_rx,
                frame_tx,
                video_time_base,
                video_rx,
                video_event_tx,
            );
            decoder.process();
        })
        .unwrap();

    let waker = Arc::new(ThreadWaker::new());
    let (vo_tx, vp_rx) = crossbeam_channel::unbounded();
    let vo_tx = WakingSender::new(vo_tx, waker.clone());

    let mut tick_scheduler = VideoOutput::new(
        audio_clock.clone(),
        frame_rx.clone(),
        frame.clone(),
        current_pts.clone(),
        vp_rx,
        event_tx.clone(),
        external_callback.cloned(),
    );
    let output_handle = std::thread::spawn(move || {
        waker.set();
        tick_scheduler.process();
    });
    (dec_handle, output_handle, video_tx, vo_tx, video_packet_tx)
}
