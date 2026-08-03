// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    thread::{self, JoinHandle},
};

use crossbeam_channel::Sender;
use gilia_windows::{
    Com, EventReceiver, MmcssRegistration, WasapiError, WasapiEvent, WasapiStream, w,
};
use tracing::warn;

use crate::{
    PlayerEvent::Internal,
    audio::queue::Consumer,
    engine::{ExternalCallback, ExternalEvent, get_engine_start},
    session::{AbEffect, InternalEvent::Drained, PlayerEvent, Worker},
    utils::Clock,
};

pub fn spawn(
    mut consumer: Consumer,
    effect_rx: EventReceiver<AbEffect>,
    event_tx: Sender<PlayerEvent>,
    rate: u32,
    clock: Arc<Clock>,
    volume: Arc<Mutex<Vec<f32>>>,
    callback: ExternalCallback,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let _com = Com::init().unwrap();
        let _mmcss = MmcssRegistration::register(w!("Pro Audio")).ok();

        let initial_volume = volume.lock().unwrap().first().copied();
        let mut stream = match WasapiStream::new(rate, initial_volume) {
            Ok(stream) => stream,
            Err(e) => {
                warn!("{e}");
                clock.active.store(false, Ordering::Relaxed);
                return;
            }
        };

        let mut draining = false;

        reconnect(&mut stream, &clock, &mut consumer);

        loop {
            let event = match stream.wait(&effect_rx) {
                Ok(event) => event,
                Err(WasapiError::Disconnected) => break,
                Err(e) => {
                    warn!("{e}");
                    clock.active.store(false, Ordering::Relaxed);
                    break;
                }
            };
            match event {
                WasapiEvent::Control(effect) => match effect {
                    AbEffect::Output(active) => {
                        if !stream.set_active(active) || (!active && !stream.reset()) {
                            reconnect(&mut stream, &clock, &mut consumer);
                        }
                    }
                    AbEffect::FlushConsumers(flush_tx) => {
                        unsafe {
                            consumer.clear();
                        }
                        if !stream.reset() {
                            reconnect(&mut stream, &clock, &mut consumer);
                        }
                        draining = false;
                        let _ = flush_tx.send(Worker::AudioOutput);
                    }
                    AbEffect::Reinit(rate) => {
                        let connected = stream.set_rate(rate);
                        sync_connection(connected, &clock, &mut consumer);
                    }
                    AbEffect::ModifyVolume(volumes) => {
                        let vol = volumes.first().copied().unwrap_or(1.0);
                        {
                            let mut guard = volume.lock().unwrap();
                            guard.clear();
                            guard.extend_from_slice(&volumes);
                            (callback)(ExternalEvent::VolumesChanged(guard.clone()));
                        }
                        if !stream.set_volume(vol) {
                            reconnect(&mut stream, &clock, &mut consumer);
                        }
                    }
                    AbEffect::DrainOutput => {
                        draining = true;
                        if consumer.is_empty()
                            && stream_is_empty(&mut stream, &clock, &mut consumer)
                        {
                            let _ = event_tx.send(Internal(Drained(Worker::AudioOutput)));
                            draining = false;
                        }
                    }
                },
                WasapiEvent::Audio => {
                    if draining && consumer.is_empty() {
                        if stream_is_empty(&mut stream, &clock, &mut consumer) {
                            let _ = event_tx.send(Internal(Drained(Worker::AudioOutput)));
                            draining = false;
                        }
                        continue;
                    }

                    match fill_audio_buffer(&mut stream, &mut consumer, &clock) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(e) => {
                            warn!("{e}");
                            reconnect(&mut stream, &clock, &mut consumer);
                            continue;
                        }
                    }

                    if draining
                        && consumer.is_empty()
                        && stream_is_empty(&mut stream, &clock, &mut consumer)
                    {
                        let _ = event_tx.send(Internal(Drained(Worker::AudioOutput)));
                        draining = false;
                    }
                }
                WasapiEvent::Volume(vol) => {
                    publish_volume(&volume, &callback, vol);
                }
                WasapiEvent::Connected(connected) => {
                    sync_connection(connected, &clock, &mut consumer);
                    if draining
                        && consumer.is_empty()
                        && stream_is_empty(&mut stream, &clock, &mut consumer)
                    {
                        let _ = event_tx.send(Internal(Drained(Worker::AudioOutput)));
                        draining = false;
                    }
                }
            }
        }
    })
}

fn fill_audio_buffer(
    stream: &mut WasapiStream,
    consumer: &mut Consumer,
    clock: &Clock,
) -> Result<bool, WasapiError> {
    let Some(mut buffer) = stream.acquire()? else {
        return Ok(false);
    };

    let timing = buffer.timing();
    let (filled_frames, pts_ns) = match consumer.fill(timing.rate as usize, buffer.data_mut()) {
        Some((frames, pts_ns)) => (frames, Some(pts_ns)),
        None => (0, None),
    };

    if let Err(e) = buffer.commit(filled_frames) {
        warn!("{e}");
        reconnect(stream, clock, consumer);
        return Ok(false);
    }

    if let Some(pts_ns) = pts_ns {
        let now = get_engine_start().elapsed().as_nanos() as i64;
        let written_ns = filled_frames as i64 * 1_000_000_000 / timing.rate as i64;
        clock.update(pts_ns, now + timing.delay_ns + written_ns);
    }

    Ok(true)
}

fn reconnect(stream: &mut WasapiStream, clock: &Clock, consumer: &mut Consumer) {
    let connected = stream.connect();
    sync_connection(connected, clock, consumer);
}

fn stream_is_empty(stream: &mut WasapiStream, clock: &Clock, consumer: &mut Consumer) -> bool {
    match stream.is_empty() {
        Ok(is_empty) => is_empty,
        Err(e) => {
            warn!("{e}");
            reconnect(stream, clock, consumer);
            false
        }
    }
}

fn sync_connection(connected: bool, clock: &Clock, consumer: &mut Consumer) {
    clock.active.store(connected, Ordering::Relaxed);
    consumer.drain_until(clock.get_ms());
}

fn publish_volume(volume: &Arc<Mutex<Vec<f32>>>, callback: &ExternalCallback, vol: f32) {
    let mut guard = volume.lock().unwrap();
    if guard.is_empty() {
        guard.resize(2, vol);
    } else {
        guard.fill(vol);
    }
    (callback)(ExternalEvent::VolumesChanged(guard.clone()));
}
