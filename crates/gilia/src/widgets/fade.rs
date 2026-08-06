// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use iced::{
    Color, Length, Point, Rectangle, Renderer, Size, Theme, mouse,
    widget::{
        self,
        canvas::{self, Geometry, gradient::Linear},
    },
};

#[derive(Debug, Clone, Copy)]
struct BottomFade;

pub(crate) fn fade<Message: 'static>() -> impl iced::Widget<Message> {
    widget::canvas(BottomFade)
        .width(Length::Fill)
        .height(Length::Fill)
}

impl<Message> canvas::Program<Message> for BottomFade {
    type State = canvas::Cache;

    fn draw(
        &self,
        cache: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let geometry = cache.draw(renderer, bounds.size(), |frame| {
            let bottom_height = (bounds.height * 0.18)
                .clamp(120.0, 240.0)
                .min(bounds.height);
            let top = bounds.height - bottom_height;

            let gradient = Linear::new(Point::new(0.0, bounds.height), Point::new(0.0, top))
                .add_stop(0.0, Color::from_rgba(0.0, 0.0, 0.0, 0.68))
                .add_stop(1.0, Color::TRANSPARENT);

            frame.fill_rectangle(
                Point::new(0.0, top),
                Size::new(bounds.width, bottom_height),
                gradient,
            );
        });

        vec![geometry]
    }
}
