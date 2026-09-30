// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    thread::JoinHandle,
};

use ffmpeg_next::{Stream, codec::Context};

use crate::{
    PlayerEvent,
    audio::{decoder::AudioDecoder, queue::queue},
    channel::{self, Sender},
    engine::{EngineError, ExternalCallback},
    session::{AbEffect, DecoderEffect},
    utils::{Clock, TrackedPacket},
};

#[cfg(target_os = "linux")]
pub(crate) mod ab_pipewire;

#[cfg(target_os = "windows")]
pub(crate) mod ab_wasapi;

pub(crate) mod decoder;
pub(crate) mod queue;

pub type AudioBackendSender = channel::Sender<AbEffect>;

pub fn spawn_audio_stream(
    audio_stream: &Stream,
    event_tx: &Sender<PlayerEvent>,
    external_callback: &ExternalCallback,
    volume: &Arc<Mutex<Vec<f32>>>,
    audio_clock: Arc<Clock>,
    path: &str,
) -> Result<
    (
        JoinHandle<()>,
        JoinHandle<()>,
        Sender<DecoderEffect>,
        AudioBackendSender,
        Sender<TrackedPacket>,
    ),
    EngineError,
> {
    #[cfg(target_os = "linux")]
    let (ab_sender, ab_receiver) = channel::Builder::unbounded().pipewire().unwrap();
    #[cfg(windows)]
    let (ab_sender, ab_receiver) = channel::Builder::unbounded().wasapi().unwrap();
    let (audio_packet_tx, audio_packet_rx) = channel::Builder::unbounded().channel();

    audio_clock.active.store(true, Ordering::Relaxed);

    let audio_context = Context::from_parameters(audio_stream.parameters()).unwrap();
    let audio_time_base = audio_stream.time_base();
    let decoder = audio_context.decoder().audio().unwrap();
    let audio_rate = decoder.rate();
    let (audio_tx, audio_rx) = channel::Builder::unbounded().channel();
    let (audio_buffer_tx, audio_buffer_rx) = queue(128);
    let audio_event_tx = event_tx.clone();
    let dec_audio_clock = audio_clock.clone();
    let dec_handle = std::thread::Builder::new()
        .name("audio-decoder".into())
        .spawn(move || {
            let mut audio_decoder = AudioDecoder::new(
                decoder,
                audio_packet_rx,
                audio_buffer_tx,
                audio_time_base,
                audio_rx,
                audio_event_tx,
                dec_audio_clock,
            );
            audio_decoder.process();
        })
        .unwrap();

    let ab_event_tx = event_tx.clone();

    #[cfg(target_os = "linux")]
    let ab_handle = ab_pipewire::spawn(
        audio_buffer_rx,
        ab_receiver,
        ab_event_tx,
        audio_rate,
        audio_clock,
        volume.clone(),
        external_callback.clone(),
        path.to_string(),
    );

    #[cfg(target_os = "windows")]
    let ab_handle = ab_wasapi::spawn(
        audio_buffer_rx,
        ab_receiver,
        ab_event_tx,
        audio_rate,
        audio_clock,
        volume.clone(),
        external_callback.clone(),
    );

    Ok((dec_handle, ab_handle, audio_tx, ab_sender, audio_packet_tx))
}
