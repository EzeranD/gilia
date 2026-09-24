// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, atomic::Ordering};

use crossbeam_channel::Sender;
use ffmpeg_next::{format::input, media::Type};

use super::messages::{AudioChannels, SessionChannels, SubtitleChannels, VideoChannels};
use crate::{
    ActiveTracks, PlayerEvent, PlayerMeta, PlayerThreads, SharedPlayerState,
    audio::spawn_audio_stream,
    demuxer::Demuxer,
    engine::{EngineError, ExternalCallback},
    subtitle::spawn_sub_stream,
    utils::{MemoryBudget, ThreadWaker},
    video::spawn_video_stream,
};

pub fn open(
    path: &str,
    shared: &SharedPlayerState,
    external_callback: &ExternalCallback,
    event_tx: &Sender<PlayerEvent>,
) -> Result<(ActiveTracks, SessionChannels, PlayerThreads, PlayerMeta), EngineError> {
    let ictx = input(&path)?;
    let (demuxer_tx, demuxer_rx) = crossbeam_channel::unbounded();
    let mut channels = SessionChannels::new(demuxer_tx, external_callback.clone());
    let mut threads = PlayerThreads::default();

    let audio_stream = ictx.streams().best(Type::Audio);
    let video_stream = ictx.streams().best(Type::Video);
    let sub_stream = ictx.streams().best(Type::Subtitle);

    let meta = PlayerMeta::new(&ictx);

    let mut audio_idx = None;
    let mut video_idx = None;
    let mut sub_idx = None;
    let mut audio_packet_tx = None;
    let mut video_packet_tx = None;
    let mut sub_packet_tx = None;

    if let Some(audio_stream) = audio_stream {
        audio_idx = Some(audio_stream.index());
        let (dec_handle, ab_handle, dec_effect_tx, pw_tx, packet_tx) = spawn_audio_stream(
            &audio_stream,
            event_tx,
            external_callback,
            &shared.audio_info.volume,
            shared.clock.clone(),
            path,
        );

        channels.audio = Some(AudioChannels {
            decoder: dec_effect_tx,
            output: pw_tx,
        });
        audio_packet_tx = Some(packet_tx);
        shared.clock.active.store(true, Ordering::Relaxed);
        threads.audio_decoder = Some(dec_handle);
        threads.audio_backend = Some(ab_handle);
    }

    if let Some(video_stream) = video_stream {
        video_idx = Some(video_stream.index());
        let (dec_handle, output_handle, dec_tx, vo_tx, packet_tx) = spawn_video_stream(
            &video_stream,
            event_tx,
            Some(external_callback),
            &shared.config,
            &shared.video_output.frame,
            &shared.video_output.current_pts,
            &shared.clock,
        );
        channels.video = Some(VideoChannels {
            decoder: dec_tx,
            output: vo_tx,
        });
        video_packet_tx = Some(packet_tx);
        threads.video_decoder = Some(dec_handle);
        threads.video_output = Some(output_handle);
    }

    if let Some(sub_stream) = sub_stream {
        sub_idx = Some(sub_stream.index());
        let (dec_handle, sub_tx, packet_tx) = spawn_sub_stream(
            &ictx,
            event_tx,
            &sub_stream,
            shared.clock.clone(),
            shared.sub_output.track.clone(),
            shared.sub_output.renderer.clone(),
        );
        channels.sub = Some(SubtitleChannels { decoder: sub_tx });
        sub_packet_tx = Some(packet_tx);
        threads.sub_decoder = Some(dec_handle);
    }

    let demuxer_event_tx = event_tx.clone();
    let demuxer_waker = Arc::new(ThreadWaker::new());
    let memory_budget = MemoryBudget::new(150 * 1024 * 1024, demuxer_waker.clone());
    let mut demuxer = Demuxer::new(
        ictx,
        video_idx,
        audio_idx,
        sub_idx,
        video_packet_tx,
        audio_packet_tx,
        sub_packet_tx,
        demuxer_rx,
        demuxer_event_tx,
        shared.clock.clone(),
        demuxer_waker,
        memory_budget,
    );
    let demuxer_handle = std::thread::Builder::new()
        .name("demuxer".into())
        .spawn(move || {
            demuxer.waker.set();
            demuxer.read_packets();
        })
        .unwrap();
    threads.demuxer = Some(demuxer_handle);

    Ok((
        ActiveTracks {
            subs: sub_idx,
            audio: audio_idx,
            video: video_idx,
        },
        channels,
        threads,
        meta,
    ))
}
