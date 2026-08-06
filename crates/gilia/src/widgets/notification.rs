// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::{Duration, Instant};

use iced::{
    Event, Length, Rectangle, Renderer, Size, Theme,
    advanced::{Layout, Shell, Widget, layout, mouse, renderer, widget::Tree},
    widget::{Container, container, text},
};

pub(crate) struct Notification {
    text: String,
    expires_at: Instant,
}

pub(crate) struct TextOverlay<'a> {
    content: Container<'a, iced::widget::Text<'a>>,
    expires_at: Option<Instant>,
}

pub(crate) fn text_overlay(notification: Option<&Notification>) -> TextOverlay<'_> {
    TextOverlay {
        content: container(text(notification.map_or("", |noti| &noti.text)).size(25))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(10),
        expires_at: notification.map(|noti| noti.expires_at),
    }
}

impl Notification {
    pub(crate) fn new(text: String, duration: Duration) -> Self {
        Self {
            text,
            expires_at: Instant::now() + duration,
        }
    }
}

impl iced::advanced::widget::Meta for TextOverlay<'_> {}

impl<Message> Widget<Message, Theme, Renderer> for TextOverlay<'_> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children::<Message, Theme, Renderer>(std::slice::from_mut(&mut self.content));
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) {
        Widget::<Message, Theme, Renderer>::layout(
            &mut self.content,
            &mut tree.children[0],
            renderer,
            limits,
        );
        tree.size = tree.children[0].size;
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if self
            .expires_at
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            Widget::<Message, Theme, Renderer>::draw(
                &self.content,
                &tree.children[0],
                renderer,
                theme,
                style,
                layout,
                cursor,
                viewport,
            );
        }
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        _event: &Event,
        _layout: Layout,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        if let Some(deadline) = self.expires_at
            && deadline > Instant::now()
        {
            shell.request_redraw_at(deadline);
        }
    }
}
