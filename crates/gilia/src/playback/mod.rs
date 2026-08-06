// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{fmt::Write, time::Duration};

use gilia::{
    ChapterSeek, Player, SeekMode,
    engine::{
        ExternalEvent::{self, Opened, TrackChanged, VolumesChanged},
        PlaybackMode, SessionConfig, TrackKind,
    },
};
use iced::{
    Length, Task, Widget,
    futures::{self, SinkExt},
    keyboard::{
        self,
        Key::{self, Character, Named},
        Modifiers,
        key::Named::{ArrowDown, ArrowLeft, ArrowRight, ArrowUp, Home, Space},
    },
    mouse::ScrollDelta,
    widget::{container, stack, text},
};
use keepawake::KeepAwake;
use tracing::error;

use crate::{
    playback::controls::{Controls, MAX_VOLUME},
    widgets::notification::{Notification, text_overlay},
};

mod controls;

pub(super) struct Playback {
    player: Player,
    load_state: LoadState,
    hovered: bool,
    controls: Controls,
    notification: Option<Notification>,
    volume: f32,
    first_volume_change: bool,
    keepawake: Option<KeepAwake>,
    eof_loop: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadState {
    Unopened,
    Opening,
    Ready,
}

#[derive(Debug, Clone)]
pub(super) enum Message {
    CallbackEvent(ExternalEvent),
    PositionTick,
    HideControls,
    SeekTimer,
    Hovered(bool),
    Action(Action),
    Shortcut(Action),
    Scroll(ScrollDelta),
    PointerMoved,
    ControlsHovered(bool),
    PopupOpen(bool),
    ToggleTime,
}

#[derive(Debug, Clone)]
enum Action {
    TogglePlayback,
    ToggleFullscreen,
    AdjustVolume(f32),
    SetVolume(f32),
    SeekRelative(i64),
    Restart,
    SeekChapter(ChapterSeek),
    Scrub(i64),
    EndScrub,
    CycleTrack(TrackKind),
    SelectTrack(TrackKind, usize),
    ToggleLoop,
    Exit,
}

impl Playback {
    pub(super) fn new(config: SessionConfig) -> (Self, Task<crate::Message>) {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = move |event| {
            event_tx.send(event).ok();
        };
        let event_task = Task::stream(iced::stream::channel(
            100,
            move |mut sender: futures::channel::mpsc::Sender<Message>| async move {
                while let Some(event) = event_rx.recv().await {
                    sender.send(Message::CallbackEvent(event)).await.ok();
                }
            },
        ));
        let player = Player::new(config, cb);
        (
            Self {
                player,
                load_state: LoadState::Unopened,
                hovered: false,
                controls: Controls::default(),
                notification: None,
                volume: 1.0,
                first_volume_change: true,
                keepawake: None,
                eof_loop: false,
            },
            event_task.map(crate::Message::Playback),
        )
    }

    pub(super) fn subscription(&self) -> iced::Subscription<crate::Message> {
        let mut subscriptions = vec![];
        subscriptions.push(keyboard::listen().filter_map(|event| {
            if let keyboard::Event::KeyPressed { key, modifiers, .. } = event {
                Action::from_key(&key, modifiers).map(Message::Shortcut)
            } else {
                None
            }
        }));

        if let Some(metadata) = self.player.metadata()
            && metadata
                .tracks
                .iter()
                .all(|track| track.kind != TrackKind::Video)
        {
            subscriptions
                .push(iced::time::every(Duration::from_millis(250)).map(|_| Message::PositionTick));
        }

        if let Some(deadline) = self.controls.hide_deadline() {
            subscriptions.push(iced::Subscription::run_with(deadline, |deadline| {
                let deadline = *deadline;
                futures::stream::once(async move {
                    tokio::time::sleep_until(deadline.into()).await;
                    Message::HideControls
                })
            }));
        }

        if let Some(deadline) = self.player.seek_deadline() {
            subscriptions.push(iced::Subscription::run_with(deadline, |deadline| {
                let deadline = *deadline;
                futures::stream::once(async move {
                    tokio::time::sleep_until(deadline.into()).await;
                    Message::SeekTimer
                })
            }));
        }

        iced::Subscription::batch(subscriptions).map(crate::Message::Playback)
    }

    pub(super) fn unopened(&self) -> bool {
        self.load_state == LoadState::Unopened
    }

    pub(super) fn view(&self, fullscreen: bool) -> impl Widget<crate::Message> {
        let content = match self.load_state {
            LoadState::Unopened => iced::widget::space().boxed(),
            LoadState::Opening => text("Opening…").boxed(),
            LoadState::Ready => {
                let video = self.player.view();
                let notification = text_overlay(self.notification.as_ref());
                let controls = self.controls.view(&self.player, self.volume, fullscreen);

                stack![video, notification, controls].boxed()
            }
        };

        container(content)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_| container::Style {
                background: Some(iced::Background::Color(iced::Color::BLACK)),
                ..Default::default()
            })
            .map(crate::Message::Playback)
            .boxed()
    }

    pub(super) fn open(&mut self, path: String) {
        if !self.unopened() {
            return;
        }

        if let Err(error) = self.player.open(path) {
            error!("Failed to start opening a session: {error}");
            return;
        }

        self.load_state = LoadState::Opening;
        self.hovered = false;
        self.controls = Controls::default();
        self.notification = None;
        self.volume = 1.0;
        self.first_volume_change = true;
        self.keepawake = None;
    }

    pub(super) fn update(&mut self, message: Message) -> Task<crate::Message> {
        match message {
            Message::CallbackEvent(event) => {
                self.player.handle_event(&event);
                self.callback_event(event).map(crate::Message::Playback)
            }
            Message::PositionTick => Task::none(),
            Message::HideControls => {
                self.controls.poll_hide();
                Task::none()
            }
            Message::SeekTimer => {
                self.player.poll_seek();
                Task::none()
            }
            Message::Hovered(hovered) => {
                self.hovered = hovered;
                if hovered {
                    self.controls.activity();
                }
                Task::none()
            }
            Message::Action(action) => self.handle_action(action),
            Message::Shortcut(action) => {
                if self.hovered {
                    self.handle_action(action)
                } else {
                    Task::none()
                }
            }
            Message::Scroll(delta) => match delta {
                ScrollDelta::Lines { y, .. } if y > 0.0 => {
                    self.handle_action(Action::AdjustVolume(0.02))
                }
                ScrollDelta::Lines { y, .. } if y < 0.0 => {
                    self.handle_action(Action::AdjustVolume(-0.02))
                }
                _ => Task::none(),
            },
            Message::PointerMoved => {
                if self.hovered {
                    self.controls.activity();
                }

                Task::none()
            }
            Message::ControlsHovered(hovered) => {
                self.controls.set_hovered(hovered);
                Task::none()
            }
            Message::PopupOpen(open) => {
                self.controls.set_popup(open);
                Task::none()
            }
            Message::ToggleTime => {
                self.controls.toggle_time();
                Task::none()
            }
        }
    }

    fn handle_action(&mut self, action: Action) -> Task<crate::Message> {
        match action {
            Action::TogglePlayback => match self.player.toggle_playback() {
                PlaybackMode::Playing => {
                    self.keepawake = keepawake::Builder::default()
                        .display(true)
                        .idle(true)
                        .create()
                        .ok();
                }
                PlaybackMode::Paused => {
                    self.keepawake = None;
                }
            },
            Action::ToggleFullscreen => return Task::done(crate::Message::ToggleFullscreen),
            Action::AdjustVolume(change) => {
                self.volume = self
                    .player
                    .modify_volumes(|volume| (volume + change).clamp(0.0, MAX_VOLUME));

                let text = format!("Volume {}%", (self.volume * 100.0).round() as u32);
                self.set_text(text, Duration::from_secs(2));
            }
            Action::SetVolume(volume) => {
                self.volume = self.player.modify_volumes(|_| volume);

                let text = format!("Volume {}%", (self.volume * 100.0).round() as u32);
                self.set_text(text, Duration::from_secs(2));
            }
            Action::SeekRelative(offset_ms) => {
                self.controls.cancel_scrub();
                self.controls.refresh_activity();
                self.player.seek_rel(offset_ms, SeekMode::Throttled);
            }
            Action::Restart => {
                self.controls.cancel_scrub();
                self.controls.refresh_activity();
                self.player.seek_to(0, SeekMode::Throttled);
            }
            Action::SeekChapter(chapter) => {
                self.controls.cancel_scrub();
                self.controls.refresh_activity();
                self.player.seek_chapter(chapter, SeekMode::Throttled);
            }
            Action::Scrub(position_ms) => {
                self.controls.set_scrub(Some(position_ms));
                self.controls.refresh_activity();
                self.player.seek_to(position_ms, SeekMode::Throttled);
            }
            Action::EndScrub => {
                self.controls.cancel_scrub();
            }
            Action::CycleTrack(kind) => {
                let active_tracks = self.player.active_tracks();
                let current = match kind {
                    TrackKind::Audio => active_tracks.audio,
                    TrackKind::Subtitle => active_tracks.subs,
                    TrackKind::Video => active_tracks.video,
                };

                if let Some(next) = self
                    .player
                    .metadata()
                    .and_then(|metadata| metadata.next_track(kind, current))
                {
                    self.player.select_track(kind, next.id);
                }
            }
            Action::SelectTrack(kind, id) => {
                self.player.select_track(kind, id);
            }
            Action::ToggleLoop => {
                self.eof_loop = !self.eof_loop;
                self.set_text(format!("Loop: {}", self.eof_loop), Duration::from_secs(2));
            }
            Action::Exit => return Task::done(crate::Message::Exit),
        }

        Task::none()
    }

    fn callback_event(&mut self, event: ExternalEvent) -> Task<Message> {
        match event {
            Opened(_) => {
                self.load_state = LoadState::Ready;
                self.keepawake = keepawake::Builder::default()
                    .display(true)
                    .idle(true)
                    .create()
                    .ok();
                Task::none()
            }
            ExternalEvent::Error(_) => {
                self.load_state = LoadState::Unopened;
                self.keepawake = None;
                Task::none()
            }
            ExternalEvent::Eof => {
                if self.player.metadata().is_some() {
                    if self.eof_loop {
                        self.player.seek_to(0, SeekMode::Immediate);
                        self.player.play();
                    } else {
                        self.keepawake = None;
                    }
                }
                Task::none()
            }
            TrackChanged(kind, id) => {
                let Some(meta) = self.player.metadata() else {
                    return Task::none();
                };

                let Some(track_meta) = meta.get_track(id) else {
                    return Task::none();
                };

                let label = match kind {
                    TrackKind::Audio => "Audio",
                    TrackKind::Video => "Video",
                    TrackKind::Subtitle => "Subtitles",
                };

                let mut text = format!("{label}: ({id})");

                if let Some(title) = &track_meta.title {
                    let _ = write!(text, "\"{title}\"");
                }

                if let Some(lang) = &track_meta.lang {
                    let _ = write!(text, " ({lang})");
                }

                self.set_text(text, Duration::from_secs(2));
                Task::none()
            }
            VolumesChanged(volumes) => {
                let channel1_vol = volumes.first().copied().unwrap_or(0.0);
                self.volume = channel1_vol;

                if self.first_volume_change {
                    self.first_volume_change = false;
                    return Task::none();
                }

                let text = format!("Volume {}%", (channel1_vol * 100.0).round() as u32);
                self.set_text(text, Duration::from_secs(2));
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn set_text(&mut self, text: String, duration: Duration) {
        self.notification = Some(Notification::new(text, duration));
    }
}

impl Action {
    fn from_key(key: &Key, modifiers: Modifiers) -> Option<Self> {
        match key.as_ref() {
            Named(ArrowUp) => Some(Self::AdjustVolume(0.02)),
            Named(ArrowDown) => Some(Self::AdjustVolume(-0.02)),
            Named(ArrowLeft) => Some(Self::SeekRelative(-5000)),
            Named(ArrowRight) => Some(Self::SeekRelative(5000)),
            Named(Home) => Some(Self::Restart),
            Character("k") if modifiers.shift() => Some(Self::SeekChapter(ChapterSeek::Current)),
            Character("j") if modifiers.shift() => Some(Self::SeekChapter(ChapterSeek::Previous)),
            Character("l") if modifiers.shift() => Some(Self::SeekChapter(ChapterSeek::Next)),
            Named(Space) | Character("k") => Some(Self::TogglePlayback),
            Character("j") => Some(Self::SeekRelative(-10000)),
            Character("l") => Some(Self::SeekRelative(10000)),
            Character("a") => Some(Self::CycleTrack(TrackKind::Audio)),
            Character("s") => Some(Self::CycleTrack(TrackKind::Subtitle)),
            Character("v") => Some(Self::CycleTrack(TrackKind::Video)),
            Character("f") => Some(Self::ToggleFullscreen),
            Character("r") if modifiers.shift() => Some(Self::ToggleLoop),
            Character("q") => Some(Self::Exit),
            _ => None,
        }
    }
}
