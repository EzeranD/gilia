// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    sync::{Arc, Mutex, atomic::AtomicI64},
    thread::JoinHandle,
};

use ffmpeg_next::Stream;

use crate::{
    PlayerEvent, SessionConfig, VideoFrame,
    channel::{self, Sender},
    session::{DecoderEffect, ExternalCallback, SessionError, VoEffect},
    utils::{Clock, TrackedPacket},
    video::{decoder::VideoDecoder, output::VideoOutput},
};

pub mod decoder;
pub mod frame;
mod hw_ffmpeg;
pub mod output;

pub type VideoOutputSender = channel::Sender<VoEffect>;
pub type VideoOutputReceiver = channel::Receiver<VoEffect>;

pub fn spawn_video_stream(
    video_stream: &Stream,
    event_tx: &Sender<PlayerEvent>,
    external_callback: &ExternalCallback,
    config: &Arc<SessionConfig>,
    frame: &Arc<Mutex<Option<VideoFrame>>>,
    current_pts: &Arc<AtomicI64>,
    audio_clock: &Arc<Clock>,
) -> Result<
    (
        JoinHandle<()>,
        JoinHandle<()>,
        Sender<DecoderEffect>,
        VideoOutputSender,
        Sender<TrackedPacket>,
    ),
    SessionError,
> {
    let (vo_tx, vp_rx) = channel::Builder::unbounded().channel();

    let (frame_tx, frame_rx) = channel::Builder::bounded(3).channel();
    let (video_tx, video_rx) = channel::Builder::unbounded().channel();
    let (video_packet_tx, video_packet_rx) = channel::Builder::unbounded().channel();

    let video_time_base = video_stream.time_base();
    let parameters = video_stream.parameters();
    let video_event_tx = event_tx.clone();
    let config = config.clone();
    let decoder_clock = audio_clock.clone();
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
                decoder_clock,
            );
            decoder.process();
        })
        .unwrap();

    let mut video_output = VideoOutput::new(
        audio_clock.clone(),
        frame_rx.clone(),
        frame.clone(),
        current_pts.clone(),
        vp_rx,
        event_tx.clone(),
        external_callback.clone(),
    );
    let output_handle = std::thread::spawn(move || {
        video_output.process();
    });
    Ok((dec_handle, output_handle, video_tx, vo_tx, video_packet_tx))
}
