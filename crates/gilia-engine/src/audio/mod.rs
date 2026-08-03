// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    thread::JoinHandle,
};

use crossbeam_channel::Sender;
use ffmpeg_next::{Stream, codec::Context};

use crate::{
    PlayerEvent,
    audio::{decoder::AudioDecoder, queue::queue},
    engine::{EngineError, ExternalCallback},
    session::{AbEffect, DecoderEffect},
    utils::{Clock, ThreadWaker, TrackedPacket, UnparkSender, unpark_channel},
};

#[cfg(target_os = "linux")]
pub(crate) mod ab_pipewire;

#[cfg(target_os = "windows")]
pub(crate) mod ab_wasapi;

pub(crate) mod decoder;
pub(crate) mod queue;

#[cfg(target_os = "linux")]
pub type AudioBackendSender = pipewire::channel::Sender<AbEffect>;
#[cfg(target_os = "linux")]
pub type AudioBackendReceiver = pipewire::channel::Receiver<AbEffect>;

#[cfg(target_os = "windows")]
pub type AudioBackendSender = gilia_windows::EventSender<AbEffect>;
#[cfg(target_os = "windows")]
pub type AudioBackendReceiver = gilia_windows::EventReceiver<AbEffect>;

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
        UnparkSender<DecoderEffect>,
        AudioBackendSender,
        Sender<TrackedPacket>,
    ),
    EngineError,
> {
    let (ab_sender, ab_receiver) = ab_channel()?;
    let (audio_packet_tx, audio_packet_rx) = crossbeam_channel::unbounded();

    audio_clock.active.store(true, Ordering::Relaxed);

    let audio_context = Context::from_parameters(audio_stream.parameters()).unwrap();
    let audio_time_base = audio_stream.time_base();
    let decoder = audio_context.decoder().audio().unwrap();
    let audio_rate = decoder.rate();
    let waker = Arc::new(ThreadWaker::new());
    let (audio_tx, audio_rx) = unpark_channel(waker.clone());
    let (audio_buffer_tx, audio_buffer_rx) = queue(128, waker.clone());
    let audio_event_tx = event_tx.clone();
    let decoder_waker = waker.clone();
    let dec_audio_clock = audio_clock.clone();
    let dec_handle = std::thread::Builder::new()
        .name("audio-decoder".into())
        .spawn(move || {
            decoder_waker.set();
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

fn ab_channel() -> Result<(AudioBackendSender, AudioBackendReceiver), EngineError> {
    #[cfg(target_os = "linux")]
    {
        Ok(pipewire::channel::channel())
    }
    #[cfg(target_os = "windows")]
    {
        // TODO replace this with proper error handling
        Ok(gilia_windows::event_channel().unwrap())
    }
}
