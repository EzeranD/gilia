// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gilia::engine::{PlayerMeta, TrackKind, TrackMeta};
use iced::{
    Event, Length, Rectangle, Renderer, Size, Theme, Vector,
    advanced::{
        Layout, Shell, Widget, layout, mouse, overlay, renderer, shell, text,
        widget::{Operation, Tree, tree},
    },
    touch,
    widget::overlay::menu,
};

pub(crate) struct TrackSelector<'a, Message, W> {
    trigger: W,
    metadata: &'a PlayerMeta,
    kind: TrackKind,
    selected_id: Option<usize>,
    on_select: Option<Box<dyn Fn(usize) -> Message + 'a>>,
    on_open: Option<Message>,
    on_close: Option<Message>,
    menu_style: menu::StyleFn<'a, Theme>,
}

struct TrackMenu<'a, Message> {
    menu: overlay::Element<'a, usize, Theme, Renderer>,
    is_open: &'a mut bool,
    on_select: &'a dyn Fn(usize) -> Message,
    on_close: Option<&'a Message>,
}

#[derive(Debug, Default)]
struct State {
    is_open: bool,
    menu: menu::State,
    hovered_option: Option<usize>,
    selected_id: Option<usize>,
    options: Vec<TrackOption>,
}

#[derive(Debug, Clone)]
struct TrackOption {
    id: usize,
    label: String,
}

pub(crate) fn track_selector<Message, W>(
    trigger: W,
    metadata: &PlayerMeta,
    kind: TrackKind,
    selected_id: Option<usize>,
) -> TrackSelector<'_, Message, W> {
    TrackSelector {
        trigger,
        metadata,
        kind,
        selected_id,
        on_select: None,
        on_open: None,
        on_close: None,
        menu_style: Box::new(|theme| {
            let mut style = menu::default(theme);
            style.border.radius = 4.0.into();
            style
        }),
    }
}

fn track_options(
    metadata: &PlayerMeta,
    kind: TrackKind,
    selected_id: Option<usize>,
) -> Vec<TrackOption> {
    metadata
        .tracks(kind)
        .map(|track| TrackOption {
            id: track.id,
            label: if selected_id == Some(track.id) {
                format!("✓ {}", track_label(track))
            } else {
                track_label(track)
            },
        })
        .collect()
}

fn track_label(track: &TrackMeta) -> String {
    match (&track.title, &track.lang) {
        (Some(title), Some(lang)) => format!("{title} ({lang})"),
        (Some(title), None) => title.clone(),
        (None, Some(lang)) => format!("{lang} · {}", track.codec),
        (None, None) => format!("{} · #{}", track.codec, track.id),
    }
}

fn option_label(option: &TrackOption) -> String {
    option.label.clone()
}

impl<'a, Message, W> TrackSelector<'a, Message, W> {
    pub(crate) fn on_select(mut self, on_select: impl Fn(usize) -> Message + 'a) -> Self {
        self.on_select = Some(Box::new(on_select));
        self
    }

    pub(crate) fn on_open(mut self, message: Message) -> Self {
        self.on_open = Some(message);
        self
    }

    pub(crate) fn on_close(mut self, message: Message) -> Self {
        self.on_close = Some(message);
        self
    }
}

impl<Message, W> iced::advanced::widget::Meta for TrackSelector<'_, Message, W> {}

impl<Message, W> Widget<Message, Theme, Renderer> for TrackSelector<'_, Message, W>
where
    Message: Clone,
    W: Widget<Message, Theme, Renderer>,
{
    fn size(&self) -> Size<Length> {
        self.trigger.size()
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State {
            selected_id: self.selected_id,
            options: track_options(self.metadata, self.kind, self.selected_id),
            ..State::default()
        })
    }

    fn diff(&mut self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state.selected_id != self.selected_id {
            state.options = track_options(self.metadata, self.kind, self.selected_id);
            state.selected_id = self.selected_id;
        }

        tree.diff_children(std::slice::from_mut(&mut self.trigger));
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) {
        self.trigger.layout(&mut tree.children[0], renderer, limits);
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
        self.trigger.draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout,
        viewport: &Rectangle,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.trigger
            .operate(&mut tree.children[0], layout, viewport, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();

        if state.is_open && !cursor.is_levitating() && state.hovered_option.take().is_some() {
            shell.request_redraw();
        }

        if matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right))
        ) && self.on_select.is_some()
            && cursor.is_over(layout.bounds())
        {
            state.is_open = !state.is_open;
            state.hovered_option = None;
            let message = if state.is_open {
                &self.on_open
            } else {
                &self.on_close
            };
            if let Some(message) = message {
                shell.publish(message.clone());
            }

            shell.invalidate_overlay();
            shell.request_redraw();
            shell.capture_event();
            return;
        }

        self.trigger.update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.trigger
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout,
        renderer: &Renderer,
        _viewport: &Rectangle,
        translation: Vector,
        window: Size,
    ) -> Vec<overlay::Element<'b, Message, Theme, Renderer>> {
        let state = tree.state.downcast_mut::<State>();
        let Some(on_select) = &self.on_select else {
            return Vec::new();
        };
        if !state.is_open {
            return Vec::new();
        }
        let inset = 12.0_f32.min(window.width / 2.0);
        let width = 220.0_f32.min((window.width - inset * 2.0).max(0.0));
        let padding = iced::Padding::from([7.0, 10.0]);
        let text_size = iced::Pixels(14.0);
        let line_height = text::LineHeight::default();
        let row_height = f32::from(line_height.to_absolute(text_size)) + padding.y();
        let mut position = layout.position() + translation;
        position.x = position
            .x
            .clamp(inset, (window.width - inset - width).max(inset));

        let menu = menu::Menu::new(
            &mut state.menu,
            &state.options,
            &mut state.hovered_option,
            &option_label,
            |option| option.id,
            None,
            &self.menu_style,
        )
        .width(width)
        .padding(padding)
        .text_size(text_size)
        .line_height(line_height)
        .ellipsis(text::Ellipsis::End);

        let menu = menu.height(Length::Shrink.max(row_height * 8.0)).overlay(
            renderer,
            position,
            window,
            layout.bounds().height,
        );

        vec![overlay::Element::new(Box::new(TrackMenu {
            menu,
            is_open: &mut state.is_open,
            on_select: on_select.as_ref(),
            on_close: self.on_close.as_ref(),
        }))]
    }
}

impl<Message: Clone> overlay::Overlay<Message, Theme, Renderer> for TrackMenu<'_, Message> {
    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        cursor: mouse::Cursor,
    ) {
        self.menu.as_overlay().draw(renderer, theme, style, cursor);
    }

    fn operate(&mut self, renderer: &Renderer, operation: &mut dyn Operation) {
        self.menu.as_overlay_mut().operate(renderer, operation);
    }

    fn update(
        &mut self,
        event: &Event,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
    ) {
        let mut bus = shell::Bus::new();
        let mut local = shell.local(&mut bus);
        self.menu
            .as_overlay_mut()
            .update(event, cursor, renderer, &mut local);

        let selected = !local.is_empty();
        let dismiss = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(_))
                | Event::Touch(touch::Event::FingerPressed { .. })
        ) && !local.is_event_captured();

        shell.merge(local, self.on_select);

        if selected || dismiss {
            *self.is_open = false;
            if let Some(message) = self.on_close {
                shell.publish(message.clone());
            }

            shell.invalidate_overlay();
            shell.request_redraw();
            shell.capture_event();
        }
    }

    fn mouse_interaction(&self, cursor: mouse::Cursor, renderer: &Renderer) -> mouse::Interaction {
        self.menu.as_overlay().mouse_interaction(cursor, renderer)
    }
}
