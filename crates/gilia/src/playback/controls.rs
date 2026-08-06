// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::{Duration, Instant};

use gilia::{
    Player,
    engine::{PlaybackMode, TrackKind},
};
use iced::{
    Length, Widget,
    alignment::Vertical,
    mouse,
    widget::{column, container, mouse_area, responsive, row, slider, space, stack, text},
};

use super::{Action, Message};
use crate::widgets::{self, button};

const HIDE_AFTER: Duration = Duration::from_secs(3);
pub(super) const MAX_VOLUME: f32 = if cfg!(target_os = "windows") {
    1.0
} else {
    1.5
};

#[allow(clippy::struct_excessive_bools)]
pub(super) struct Controls {
    visible: bool,
    panel_hovered: bool,
    popup_open: bool,
    show_remaining: bool,
    scrub_position_ms: Option<i64>,
    last_activity: Instant,
}

impl Controls {
    pub(super) fn view<'a>(
        &'a self,
        player: &'a Player,
        volume: f32,
        fullscreen: bool,
    ) -> impl Widget<Message> {
        let content = if self.visible {
            stack![widgets::fade(), self.ui(player, volume, fullscreen)].boxed()
        } else {
            space().width(Length::Fill).height(Length::Fill).boxed()
        };

        let area = mouse_area(content)
            .on_enter(Message::Hovered(true))
            .on_exit(Message::Hovered(false))
            .on_move(|_| Message::PointerMoved)
            .on_press(Message::Action(Action::TogglePlayback))
            .on_scroll(Message::Scroll);

        if self.visible {
            area
        } else {
            area.interaction(mouse::Interaction::Hidden)
        }
    }

    fn ui<'a>(&'a self, player: &'a Player, volume: f32, fullscreen: bool) -> impl Widget<Message> {
        let Some(metadata) = player.metadata() else {
            return space().boxed();
        };

        let duration_ms = metadata.duration.max(0) / 1_000;
        let position = self
            .scrub_position_ms
            .unwrap_or_else(|| player.position_ms())
            .clamp(0, duration_ms);

        let timeline = widgets::timeline(position, duration_ms, &metadata.chapters)
            .on_scrub(|position| Message::Action(Action::Scrub(position)))
            .on_release(|| Message::Action(Action::EndScrub));

        let buttons = responsive(move |size| {
            let play = button::play(player.playback_mode() == PlaybackMode::Playing)
                .on_press(Message::Action(Action::TogglePlayback));

            let elapsed = text(format!("{} / ", widgets::format_time(position)));
            let end = button::time(position, duration_ms, self.show_remaining)
                .on_press(Message::ToggleTime);
            let time = row![elapsed, end];

            let video_count = metadata.tracks(TrackKind::Video).count();
            let threshold = if video_count > 1 { 485.0 } else { 455.0 };
            let ends_at = (duration_ms > 0 && size.width >= threshold).then(|| {
                text(widgets::format_ends_at(
                    duration_ms.saturating_sub(position),
                ))
                .size(14)
            });

            let mut right = row![space().width(Length::Fill)]
                .align_y(Vertical::Center)
                .spacing(8);

            let active_tracks = player.active_tracks();
            for (kind, selected_id) in [
                (TrackKind::Video, active_tracks.video),
                (TrackKind::Audio, active_tracks.audio),
                (TrackKind::Subtitle, active_tracks.subs),
            ] {
                let count = if kind == TrackKind::Video {
                    video_count
                } else {
                    metadata.tracks(kind).count()
                };
                if count == 0 || (kind == TrackKind::Video && count == 1) {
                    continue;
                }

                let selector = button::track_button(
                    metadata,
                    kind,
                    selected_id,
                    Message::Action(Action::CycleTrack(kind)),
                )
                .on_open(Message::PopupOpen(true))
                .on_close(Message::PopupOpen(false))
                .on_select(move |id| Message::Action(Action::SelectTrack(kind, id)));
                right = right.push(selector.boxed());
            }

            let volume = slider(0.0..=MAX_VOLUME, volume.clamp(0.0, MAX_VOLUME), |volume| {
                Message::Action(Action::SetVolume(volume))
            })
            .step(0.01)
            .width((size.width - threshold + 80.0).clamp(80.0, 120.0))
            .height(16);

            let fullscreen =
                button::fullscreen(fullscreen).on_press(Message::Action(Action::ToggleFullscreen));

            let left = row![play, time, ends_at]
                .align_y(Vertical::Center)
                .spacing(8);

            right = right.push(volume.boxed()).push(fullscreen.boxed());

            row![left, right]
                .width(Length::Fill)
                .align_y(iced::alignment::Vertical::Center)
                .spacing(8)
                .padding([2, 4])
        })
        .height(Length::Fit);

        let content = column![timeline, buttons]
            .width(Length::Fill)
            .padding([6, 4])
            .boxed();

        let area = mouse_area(content)
            .on_enter(Message::ControlsHovered(true))
            .on_exit(Message::ControlsHovered(false));

        container(area)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_bottom(Length::Fill)
            .boxed()
    }

    pub(super) fn hide_deadline(&self) -> Option<Instant> {
        (self.visible
            && !self.panel_hovered
            && !self.popup_open
            && self.scrub_position_ms.is_none())
        .then_some(self.last_activity + HIDE_AFTER)
    }

    pub(super) fn activity(&mut self) {
        self.visible = true;
        self.last_activity = Instant::now();
    }

    pub(super) fn refresh_activity(&mut self) {
        if self.visible {
            self.last_activity = Instant::now();
        }
    }

    pub(super) fn poll_hide(&mut self) {
        if let Some(deadline) = self.hide_deadline()
            && Instant::now() >= deadline
        {
            self.visible = false;
        }
    }

    pub(super) fn set_hovered(&mut self, hovered: bool) {
        self.panel_hovered = hovered;
    }

    pub(super) fn set_scrub(&mut self, position_ms: Option<i64>) {
        self.scrub_position_ms = position_ms;
    }

    pub(super) fn cancel_scrub(&mut self) {
        self.scrub_position_ms = None;
    }

    pub(super) fn toggle_time(&mut self) {
        self.show_remaining = !self.show_remaining;
    }

    pub(super) fn set_popup(&mut self, open: bool) {
        self.popup_open = open;

        if !open {
            self.activity();
        }
    }
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            visible: false,
            panel_hovered: false,
            popup_open: false,
            show_remaining: false,
            scrub_position_ms: None,
            last_activity: Instant::now(),
        }
    }
}
