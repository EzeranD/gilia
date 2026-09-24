// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    cell::RefCell,
    io::Cursor,
    rc::Rc,
    slice,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
};

use crossbeam_channel::Sender;
use libspa_sys::SPA_PROP_channelVolumes;
use pipewire::{
    context::ContextRc,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
        self,
        param::audio::{AudioFormat, AudioInfoRaw},
        pod::{Object, Pod, serialize::PodSerializer},
        utils::Direction,
    },
    stream::{StreamFlags, StreamRc},
    sys::pw_stream_get_nsec,
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
    mut audio_buffer: Consumer,
    ab_effect_rx: pipewire::channel::Receiver<AbEffect>,
    ab_event_tx: Sender<PlayerEvent>,
    rate: u32,
    audio_clock: Arc<Clock>,
    audio_volume: Arc<Mutex<Vec<f32>>>,
    callback: ExternalCallback,
    path: String,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mainloop = MainLoopRc::new(None).unwrap();
        let context = ContextRc::new(&mainloop, None).unwrap();
        let core = context.connect_rc(None).unwrap();
        let stream = StreamRc::new(
            core,
            "Gilia",
            properties! {
                *pipewire::keys::NODE_NAME => "Gilia",
                *pipewire::keys::NODE_DESCRIPTION => "Gilia",
                *pipewire::keys::APP_NAME => "Gilia",
                *pipewire::keys::MEDIA_NAME => path,
                *pipewire::keys::MEDIA_TYPE => "Audio",
                *pipewire::keys::MEDIA_ROLE => "Movie",
                *pipewire::keys::MEDIA_CATEGORY => "Playback",
            },
        )
        .unwrap();
        let values = audio_info(rate);
        let mut params = [Pod::from_bytes(&values).unwrap()];
        let current_rate = Arc::new(AtomicU32::new(rate));
        let drain_flag = Arc::new(AtomicBool::new(false));
        let pw_audio_buffer = audio_buffer.clone();
        let pw_event_tx = ab_event_tx.clone();
        let process_rate = current_rate.clone();
        let negotiated_rate = current_rate.clone();
        let recv_stream = stream.clone();
        let recv_drain = drain_flag.clone();
        let expected_rate = Rc::new(RefCell::new(Some(rate)));
        let recv_expected_rate = expected_rate.clone();
        let _receiver = ab_effect_rx.attach(mainloop.loop_(), {
            move |e| match e {
                AbEffect::ModifyVolume(mut volumes) => {
                    for v in &mut volumes {
                        *v = (*v).clamp(0.0, 1.5).powi(3);
                    }
                    recv_stream
                        .set_control(libspa_sys::SPA_PROP_channelVolumes, &volumes)
                        .unwrap();
                }
                AbEffect::Output(output) => {
                    let _ = recv_stream.set_active(output);
                    if !output {
                        recv_stream.flush(false).unwrap();
                    }
                }
                AbEffect::Reinit(rate) => {
                    *recv_expected_rate.borrow_mut() = Some(rate);
                    let values = audio_info(rate);
                    let mut params = [Pod::from_bytes(&values).unwrap()];
                    if let Err(e) = recv_stream.update_params(&mut params) {
                        warn!("Failed to update audio stream: {e}");
                        *recv_expected_rate.borrow_mut() = None;
                    }
                }
                AbEffect::FlushConsumers(flush_tx) => {
                    // SAFETY: This is called after AbEffect::Output(false) so
                    // it should be safe to clear the buffer.
                    // TODO need to do some more testing but so far seems to be good
                    // just putting this here for clarity and as a reminder
                    unsafe {
                        pw_audio_buffer.clear();
                    }
                    recv_stream.flush(false).unwrap();
                    recv_drain.store(false, Ordering::Relaxed);
                    let _ = flush_tx.send(Worker::AudioOutput);
                }
                AbEffect::DrainOutput => {
                    recv_drain.store(true, Ordering::Relaxed);
                }
            }
        });

        let _listener = stream
            .add_local_listener_with_user_data(())
            .control_info(move |_, (), id, control| {
                if id == SPA_PROP_channelVolumes {
                    let volumes = unsafe {
                        slice::from_raw_parts((*control).values, (*control).n_values as usize)
                    };
                    let mut volumes_guard = audio_volume.lock().unwrap();
                    volumes_guard.clear();
                    volumes_guard.extend(volumes.iter().map(|f| f.powf(1.0 / 3.0)));
                    callback(ExternalEvent::VolumesChanged(volumes_guard.clone()));
                }
            })
            .param_changed(move |_, (), id, pod| {
                if id != libspa_sys::SPA_PARAM_Format {
                    return;
                }

                let Some(pod) = pod else {
                    return;
                };

                let mut audio_info = AudioInfoRaw::new();
                if audio_info.parse(pod).is_err() {
                    return;
                }

                let Some(expected) = *expected_rate.borrow() else {
                    return;
                };

                if audio_info.format() != AudioFormat::F32LE
                    || audio_info.channels() != 2
                    || audio_info.rate() != expected
                {
                    return;
                }

                negotiated_rate.store(audio_info.rate(), Ordering::Relaxed);
                *expected_rate.borrow_mut() = None;
            })
            .process(move |stream, ()| match stream.dequeue_buffer() {
                None => warn!("Out of buffers"),
                Some(mut buf) => {
                    let mut time: pipewire::sys::pw_time = unsafe { std::mem::zeroed() };
                    unsafe {
                        pipewire::sys::pw_stream_get_time_n(
                            stream.as_raw_ptr(),
                            &raw mut time,
                            std::mem::size_of::<pipewire::sys::pw_time>(),
                        )
                    };

                    let datas = buf.datas_mut();
                    let data = &mut datas[0];
                    let Some(target_data) = data.data() else {
                        return;
                    };

                    let rate = process_rate.load(Ordering::Relaxed);
                    if let Some((filled, pts_ns)) = audio_buffer.fill(rate as usize, target_data) {
                        let rate = rate as i64;
                        let stride = audio_buffer.stride as i64;
                        let mut end = get_engine_start().elapsed().as_nanos() as i64;
                        end += (filled as i64 * 1_000_000_000) / rate;
                        end += (time.delay * 1_000_000_000 * time.rate.num as i64)
                            / time.rate.denom as i64;
                        end += (time.queued as i64 * 1_000_000_000) / (rate * stride);
                        end += (time.buffered as i64 * 1_000_000_000) / rate;
                        end -= unsafe { pw_stream_get_nsec(stream.as_raw_ptr()) } as i64 - time.now;

                        audio_clock.update(pts_ns, end);
                        let chunk = data.chunk_mut();
                        *chunk.offset_mut() = 0;
                        *chunk.stride_mut() = audio_buffer.stride as i32;
                        *chunk.size_mut() = (filled * audio_buffer.stride) as u32;
                    } else {
                        if drain_flag.load(Ordering::Relaxed) {
                            let _ = pw_event_tx.send(Internal(Drained(Worker::AudioOutput)));
                            drain_flag.store(false, Ordering::Relaxed);
                        }
                        let chunk = data.chunk_mut();
                        *chunk.offset_mut() = 0;
                        *chunk.stride_mut() = audio_buffer.stride as i32;
                        *chunk.size_mut() = 0;
                    }
                }
            })
            .register();
        stream
            .connect(
                Direction::Output,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::RT_PROCESS | StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .unwrap();
        mainloop.run();
    })
}

fn audio_info(rate: u32) -> Vec<u8> {
    let mut audioinfo = AudioInfoRaw::new();
    audioinfo.set_channels(2);
    audioinfo.set_format(AudioFormat::F32LE);
    audioinfo.set_rate(rate);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = libspa_sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = libspa_sys::SPA_AUDIO_CHANNEL_FR;
    audioinfo.set_position(position);
    PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(Object {
            type_: libspa_sys::SPA_TYPE_OBJECT_Format,
            id: libspa_sys::SPA_PARAM_EnumFormat,
            properties: audioinfo.into(),
        }),
    )
    .unwrap()
    .0
    .into_inner()
}
