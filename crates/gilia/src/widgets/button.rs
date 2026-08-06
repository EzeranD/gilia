// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gilia::engine::{PlayerMeta, TrackKind};
use iced::{
    Color, Length, Rectangle, Renderer, Size, Theme,
    advanced::{Layout, layout, renderer, svg, widget::Tree},
    mouse,
    widget::{button, text},
};

use super::{
    format_time,
    track_selector::{TrackSelector, track_selector},
};

const PLAY: &[u8] = include_bytes!("../../../../assets/svg/play.svg");
const PAUSE: &[u8] = include_bytes!("../../../../assets/svg/pause.svg");
const VIDEO: &[u8] = include_bytes!("../../../../assets/svg/video.svg");
const AUDIO: &[u8] = include_bytes!("../../../../assets/svg/audio.svg");
const SUBTITLE: &[u8] = include_bytes!("../../../../assets/svg/subtitle.svg");
const FULLSCREEN: &[u8] = include_bytes!("../../../../assets/svg/fullscreen.svg");
const MINIMIZE: &[u8] = include_bytes!("../../../../assets/svg/minimize.svg");
const CONTROL_SIZE: u32 = 22;

pub(crate) struct Icon {
    handle: svg::Handle,
    size: f32,
}

pub(crate) fn play<'a, Message>(playing: bool) -> button::Button<'a, Message, Icon> {
    let bytes = if playing { PAUSE } else { PLAY };
    button(icon(bytes, CONTROL_SIZE))
        .padding(3)
        .style(button_style)
}

pub(crate) fn time<'a, Message>(
    position_ms: i64,
    duration_ms: i64,
    show_remaining: bool,
) -> button::Button<'a, Message, iced::widget::Text<'a>> {
    let end_label = if show_remaining {
        format!("-{}", format_time(duration_ms.saturating_sub(position_ms)))
    } else {
        format_time(duration_ms)
    };

    button(text(end_label)).padding(0).style(button_style)
}

pub(crate) fn track_button<Message>(
    metadata: &PlayerMeta,
    kind: TrackKind,
    selected_id: Option<usize>,
    on_press: Message,
) -> TrackSelector<'_, Message, button::Button<'_, Message, Icon>> {
    let bytes = match kind {
        TrackKind::Video => VIDEO,
        TrackKind::Audio => AUDIO,
        TrackKind::Subtitle => SUBTITLE,
    };

    let trigger = button(icon(bytes, CONTROL_SIZE))
        .padding(3)
        .style(button_style)
        .on_press(on_press);

    track_selector(trigger, metadata, kind, selected_id)
}

pub(crate) fn fullscreen<'a, Message>(fullscreen: bool) -> button::Button<'a, Message, Icon> {
    let bytes = if fullscreen { MINIMIZE } else { FULLSCREEN };
    button(icon(bytes, CONTROL_SIZE))
        .padding(3)
        .style(button_style)
}

fn button_style(theme: &Theme, status: button::Status) -> button::Style {
    button::text(
        theme,
        match status {
            button::Status::Pressed => button::Status::Hovered,
            status => status,
        },
    )
}

fn icon(bytes: &'static [u8], size: u32) -> Icon {
    Icon {
        handle: svg::Handle::from_memory(bytes),
        size: size as f32,
    }
}

impl iced::advanced::widget::Meta for Icon {}

impl<Message> iced::advanced::Widget<Message, Theme, Renderer> for Icon {
    fn size(&self) -> Size<Length> {
        Size::new(self.size.into(), self.size.into())
    }

    fn layout(&mut self, tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) {
        tree.size = layout::atomic(limits, self.size, self.size);
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        style: &renderer::Style,
        layout: Layout,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let side = self.size;
        let drawing_bounds = Rectangle {
            x: bounds.center_x() - side / 2.0,
            y: bounds.center_y() - side / 2.0,
            width: side,
            height: side,
        };

        svg::Renderer::draw_svg(
            renderer,
            svg::Svg::new(self.handle.clone())
                .color(Color {
                    a: 1.0,
                    ..style.text_color
                })
                .opacity(style.text_color.a),
            drawing_bounds,
            bounds,
        );
    }
}
