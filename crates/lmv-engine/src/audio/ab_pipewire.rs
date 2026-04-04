use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    io::Cursor,
    rc::Rc,
    slice,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

type AudioBuffer = Rc<RefCell<VecDeque<(Vec<f32>, i64)>>>;

use crossbeam_channel::{Receiver, Sender};
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
};
use tracing::warn;

use crate::{
    core::{AbEffect, PlayerEvent},
    engine::{ExternalCallback, ExternalEvent},
    sync::{AudioFrames, Clock, Nanoseconds},
};

pub fn spawn(
    audio_buffer: Receiver<(Vec<f32>, i64)>,
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
        let pending: AudioBuffer = Rc::new(RefCell::new(VecDeque::new()));
        let block_offset = Rc::new(Cell::new(0));
        let flush_flag = Rc::new(Cell::new(false));
        let drain_flag = Rc::new(Cell::new(false));
        let pw_audio_buffer = audio_buffer.clone();

        let recv_stream = stream.clone();
        let recv_block_offset = block_offset.clone();
        let recv_flush = flush_flag.clone();
        let recv_drain = drain_flag.clone();
        let listener_event = ab_event_tx.clone();
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
                    while pw_audio_buffer.try_recv().is_ok() {}
                    recv_block_offset.set(0);
                    recv_flush.set(true);
                    recv_stream.trigger_process().unwrap();
                    recv_stream.flush(false).unwrap();
                    let _ = ab_event_tx.send(PlayerEvent::AudioBackendFlushed);
                }
                AbEffect::DrainOutput => {
                    recv_drain.set(true);
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
                    if let Some(cb) = callback.as_ref() {
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
                    let drain = drain_flag.get();
                    let datas = buf.datas_mut();
                    let data = &mut datas[0];
                    let target_data = data.data().unwrap();
                    let target_f32: &mut [f32] = bytemuck::cast_slice_mut(target_data);
                    let nsamples = target_f32.len();
                    let mut pending = pending.borrow_mut();
                    if flush_flag.get() {
                        pending.clear();
                        flush_flag.set(false);
                    }
                    while let Ok((block, pts)) = audio_buffer.try_recv() {
                        pending.push_back((block, pts));
                    }
                    let total_samples = pending.iter().map(|(block, _)| block.len()).sum::<usize>();
                    if total_samples == 0 && drain {
                        let _ = listener_event.send(PlayerEvent::SamplesDrained);
                    }
                    let mut needed_samples = (nsamples).min(total_samples);
                    let mut sample_offset = 0;
                    let mut current_pts = 0;
                    while needed_samples > 0 {
                        if let Some((block, pts)) = pending.front_mut() {
                            let mut offset = block_offset.get();
                            let block_remaining = block.len() - offset;
                            let taken = (needed_samples.min(block_remaining) / 2) * 2;
                            let slice = &block[offset..offset + taken];
                            target_f32[sample_offset..sample_offset + slice.len()]
                                .copy_from_slice(slice);
                            sample_offset += slice.len();
                            needed_samples -= taken;
                            offset += taken;
                            current_pts = (*pts * 1_000_000)
                                + (offset as i64 / 2) * 1_000_000_000 / rate as i64;
                            block_offset.set(offset);
                            if offset >= block.len() {
                                pending.pop_front();
                                block_offset.set(0);
                            }
                        }
                    }

                    if sample_offset > 0 {
                        let sent_frames = AudioFrames(sample_offset as i64 / 2);
                        let mut now = Nanoseconds::from_engine_start();
                        now += Nanoseconds::from_frames(sent_frames, rate);
                        now += Nanoseconds(
                            time.delay * 1_000_000_000 * time.rate.num as i64
                                / time.rate.denom as i64,
                        );
                        now += Nanoseconds::from_frames(AudioFrames(time.queued as i64 / 8), rate);
                        now += Nanoseconds::from_frames(AudioFrames(time.buffered as i64), rate);
                        audio_clock.update(current_pts, now.0);
                    }
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = 8;
                    *chunk.size_mut() = (sample_offset * 4) as u32;
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
