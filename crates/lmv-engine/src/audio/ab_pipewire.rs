use std::{
    io::Cursor,
    slice,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
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
    core::{
        AbEffect,
        InternalEvent::{AudioBackendFlushed, SamplesDrained},
        PlayerEvent,
    },
    engine::{ExternalCallback, ExternalEvent, get_engine_start},
    sync::Clock,
};

pub fn spawn(
    mut audio_buffer: Consumer,
    ab_effect_rx: pipewire::channel::Receiver<AbEffect>,
    ab_event_tx: Sender<PlayerEvent>,
    rate: u32,
    audio_clock: Arc<Clock>,
    audio_volume: Arc<Mutex<Vec<f32>>>,
    callback: Option<ExternalCallback>,
    path: String,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mainloop = MainLoopRc::new(None).unwrap();
        let context = ContextRc::new(&mainloop, None).unwrap();
        let core = context.connect_rc(None).unwrap();
        let stream = StreamRc::new(
            core,
            "LMViewer",
            properties! {
                *pipewire::keys::NODE_NAME => "LMViewer",
                *pipewire::keys::NODE_DESCRIPTION => "LMViewer",
                *pipewire::keys::APP_NAME => "LMViewer",
                *pipewire::keys::MEDIA_NAME => path,
                *pipewire::keys::MEDIA_TYPE => "Audio",
                *pipewire::keys::MEDIA_ROLE => "Movie",
                *pipewire::keys::MEDIA_CATEGORY => "Playback",
            },
        )
        .unwrap();
        let mut audioinfo = AudioInfoRaw::new();
        audioinfo.set_channels(2);
        audioinfo.set_format(AudioFormat::F32LE);
        audioinfo.set_rate(rate);
        let mut position = [0; spa::param::audio::MAX_CHANNELS];
        position[0] = libspa_sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = libspa_sys::SPA_AUDIO_CHANNEL_FR;
        audioinfo.set_position(position);
        let values = PodSerializer::serialize(
            Cursor::new(Vec::new()),
            &spa::pod::Value::Object(Object {
                type_: libspa_sys::SPA_TYPE_OBJECT_Format,
                id: libspa_sys::SPA_PARAM_EnumFormat,
                properties: audioinfo.into(),
            }),
        )
        .unwrap()
        .0
        .into_inner();
        let mut params = [Pod::from_bytes(&values).unwrap()];
        let drain_flag = Arc::new(AtomicBool::new(false));
        let pw_audio_buffer = audio_buffer.clone();
        let pw_event_tx = ab_event_tx.clone();
        let recv_stream = stream.clone();
        let recv_drain = drain_flag.clone();
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
                AbEffect::FlushConsumers => {
                    // SAFETY: This is called after AbEffect::Output(false) so
                    // it should be safe to clear the buffer.
                    // TODO need to do some more testing but so far seems to be good
                    // just putting this here for clarity and as a reminder
                    unsafe {
                        pw_audio_buffer.clear();
                    }
                    recv_stream.flush(false).unwrap();
                    let _ = ab_event_tx.send(Internal(AudioBackendFlushed));
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
                    if let Some(cb) = &callback {
                        cb(ExternalEvent::VolumesChanged(volumes_guard.clone()));
                    }
                }
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
                            let _ = pw_event_tx.send(Internal(SamplesDrained));
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
