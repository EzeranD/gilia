use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    thread::JoinHandle,
};

use crossbeam_channel::Sender;
use ffmpeg_next::{Packet, Stream, codec::Context};

use crate::{
    PlayerEvent,
    audio::{self, decoder::AudioDecoder, queue::queue},
    engine::ExternalCallback,
    session::{AbEffect, DecoderEffect},
    utils::{Clock, ThreadWaker, WakingSender, channel},
};

pub(crate) mod ab_pipewire;
pub(crate) mod decoder;
pub(crate) mod queue;

pub fn spawn_audio_stream(
    audio_stream: &(Stream, usize),
    event_tx: &Sender<PlayerEvent>,
    external_callback: &ExternalCallback,
    volume: &Arc<Mutex<Vec<f32>>>,
    audio_clock: Arc<Clock>,
    path: &str,
) -> (
    JoinHandle<()>,
    JoinHandle<()>,
    WakingSender<DecoderEffect>,
    pipewire::channel::Sender<AbEffect>,
    Sender<Packet>,
) {
    let (audio_packet_tx, audio_packet_rx) = crossbeam_channel::bounded(50);
    let (pw_tx, pw_rx) = pipewire::channel::channel();

    audio_clock.active.store(true, Ordering::Relaxed);

    let audio_context = Context::from_parameters(audio_stream.0.parameters()).unwrap();
    let audio_time_base = audio_stream.0.time_base();
    let decoder = audio_context.decoder().audio().unwrap();
    let audio_rate = decoder.rate();
    let waker = Arc::new(ThreadWaker::new());
    let (audio_tx, audio_rx) = channel(waker.clone());
    let (audio_buffer_tx, audio_buffer_rx) = queue(128, waker.clone());
    let audio_event_tx = event_tx.clone();
    let decoder_waker = waker.clone();
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
            );
            audio_decoder.process();
        })
        .unwrap();
    let ab_event_tx = event_tx.clone();
    let ab_handle = audio::ab_pipewire::spawn(
        audio_buffer_rx,
        pw_rx,
        ab_event_tx,
        audio_rate,
        audio_clock.clone(),
        volume.clone(),
        Some(external_callback.clone()),
        path.to_string(),
    );
    (dec_handle, ab_handle, audio_tx, pw_tx, audio_packet_tx)
}
