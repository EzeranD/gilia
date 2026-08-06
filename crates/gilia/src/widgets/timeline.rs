// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gilia::engine::ChapterMeta;
use iced::{
    Color, Event, Length, Point, Rectangle, Renderer, Size, Theme, Vector,
    advanced::{
        Layout, Renderer as _, Shell, Widget, layout, overlay, renderer, text as renderer_text,
        widget::{Tree, tree},
    },
    border,
    mouse::{self, Cursor},
    touch,
    widget::{column, container, text},
};

use super::format_time;

const BAR_HEIGHT: f32 = 4.0;
const ACTIVE_BAR_HEIGHT: f32 = 6.0;
const HOVERED_SEGMENT_HEIGHT: f32 = 8.0;
const HANDLE_RADIUS: f32 = 6.0;
const ACTIVE_HANDLE_RADIUS: f32 = 8.0;
const HANDLE_BOTTOM_MARGIN: f32 = 2.0;
const BAR_BOTTOM_PADDING: f32 = ACTIVE_HANDLE_RADIUS + HANDLE_BOTTOM_MARGIN;
const BAR_SIDE_PADDING: f32 = ACTIVE_HANDLE_RADIUS;
const BAR_HIT_RADIUS: f32 = 10.0;

pub(crate) struct Timeline<'a, Message> {
    position_ms: i64,
    duration_ms: i64,
    chapters: &'a [ChapterMeta],
    on_scrub: Option<Box<dyn Fn(i64) -> Message + 'a>>,
    on_release: Option<Box<dyn Fn() -> Message + 'a>>,
}

#[derive(Debug, Default)]
struct State {
    drag: Option<Drag>,
    position_ms: i64,
    hover_x: Option<f32>,
    chapter_layout: ChapterLayout,
    hover_label: Option<Tree>,
}

#[derive(Debug, Default)]
struct ChapterLayout {
    bar_width: f32,
    duration_ms: i64,
    chapter_starts: Vec<i64>,
    boundaries: Vec<f32>,
}

#[derive(Debug, Clone, Copy)]
enum Drag {
    Mouse,
    Touch(touch::Finger),
}

struct HoverLabel<'a, W> {
    content: W,
    tree: &'a mut Tree,
    layout: Layout,
}

pub(crate) fn timeline<Message>(
    position_ms: i64,
    duration_ms: i64,
    chapters: &[ChapterMeta],
) -> Timeline<'_, Message> {
    Timeline {
        position_ms,
        duration_ms,
        chapters,
        on_scrub: None,
        on_release: None,
    }
}

fn local_bar_bounds(width: f32) -> (f32, f32) {
    (BAR_SIDE_PADDING, (width - 2.0 * BAR_SIDE_PADDING).max(1.0))
}

fn fill_segment(renderer: &mut Renderer, start: f32, end: f32, y: f32, height: f32, color: Color) {
    if end <= start {
        return;
    }

    renderer.fill_quad(
        renderer::Quad {
            bounds: Rectangle::new(Point::new(start, y), Size::new(end - start, height)),
            border: border::rounded((height / 2.5).min((end - start) / 2.0)),
            snap: false,
            ..Default::default()
        },
        color,
    );
}

fn is_over_bar(position: Point, bounds: Rectangle) -> bool {
    if !bounds.contains(position) {
        return false;
    }

    let position = position - Vector::new(bounds.x, bounds.y);
    let (bar_x, bar_width) = local_bar_bounds(bounds.width);
    let bar_center_y = bounds.height - BAR_BOTTOM_PADDING;
    let hit_area = Rectangle::new(
        Point::new(bar_x, bar_center_y - BAR_HIT_RADIUS),
        Size::new(bar_width, BAR_HIT_RADIUS * 2.0),
    );

    hit_area.contains(position)
}

fn is_dragging_finger(drag: Option<Drag>, finger: touch::Finger) -> bool {
    matches!(drag, Some(Drag::Touch(active)) if active == finger)
}

impl<'a, Message> Timeline<'a, Message> {
    pub(crate) fn on_scrub(mut self, on_scrub: impl Fn(i64) -> Message + 'a) -> Self {
        self.on_scrub = Some(Box::new(on_scrub));
        self
    }

    pub(crate) fn on_release(mut self, on_release: impl Fn() -> Message + 'a) -> Self {
        self.on_release = Some(Box::new(on_release));
        self
    }

    fn hover_x(&self, state: &State, width: f32) -> Option<f32> {
        if state.drag.is_some() {
            let (bar_x, bar_width) = local_bar_bounds(width);
            let percentage = state.position_ms as f32 / self.duration_ms.max(1) as f32;
            Some(bar_x + bar_width * percentage.clamp(0.0, 1.0))
        } else {
            state.hover_x
        }
    }

    fn time_at(&self, cursor_x: f32, bounds: Rectangle) -> i64 {
        let (bar_x, bar_width) = local_bar_bounds(bounds.width);
        let cursor_progress = ((cursor_x - bounds.x - bar_x) / bar_width).clamp(0.0, 1.0);
        (cursor_progress as f64 * self.duration_ms as f64).round() as i64
    }

    fn finish_drag(&self, state: &mut State, shell: &mut Shell<'_, Message>) {
        state.drag = None;

        if let Some(on_release) = &self.on_release {
            shell.publish(on_release());
        }

        shell.capture_event();
        shell.request_redraw();
    }

    fn drag_event(
        &self,
        state: &mut State,
        event: &Event,
        bounds: Rectangle,
        cursor: Cursor,
        shell: &mut Shell<'_, Message>,
    ) -> Option<f32> {
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if state.drag.is_none() =>
            {
                let position = cursor.position().filter(|p| is_over_bar(*p, bounds))?;
                state.drag = Some(Drag::Mouse);

                Some(position.x)
            }
            Event::Mouse(mouse::Event::CursorMoved { .. })
                if matches!(state.drag, Some(Drag::Mouse)) =>
            {
                cursor.position().map(|position| position.x)
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if matches!(state.drag, Some(Drag::Mouse)) =>
            {
                self.finish_drag(state, shell);

                None
            }
            Event::Touch(touch::Event::FingerPressed { id, position })
                if state.drag.is_none() && is_over_bar(*position, bounds) =>
            {
                state.drag = Some(Drag::Touch(*id));

                Some(position.x)
            }
            Event::Touch(touch::Event::FingerMoved { id, position })
                if is_dragging_finger(state.drag, *id) =>
            {
                Some(position.x)
            }
            Event::Touch(
                touch::Event::FingerLifted { id, .. } | touch::Event::FingerLost { id, .. },
            ) if is_dragging_finger(state.drag, *id) => {
                self.finish_drag(state, shell);

                None
            }
            _ => None,
        }
    }

    fn update_chapter_layout(&self, layout: &mut ChapterLayout, bar_x: f32, bar_width: f32) {
        let starts = self
            .chapters
            .iter()
            .map(|chapter| chapter.start_ms)
            .filter(|start_ms| (1..self.duration_ms).contains(start_ms));

        if layout.bar_width == bar_width
            && layout.duration_ms == self.duration_ms
            && layout.chapter_starts.iter().copied().eq(starts.clone())
        {
            return;
        }

        layout.bar_width = bar_width;
        layout.duration_ms = self.duration_ms;
        layout.chapter_starts.clear();
        layout.chapter_starts.extend(starts);

        layout.boundaries.clear();
        layout.boundaries.push(bar_x);
        layout.boundaries.extend(
            layout
                .chapter_starts
                .iter()
                .map(|start_ms| bar_x + bar_width * *start_ms as f32 / self.duration_ms as f32),
        );
        layout.boundaries.push(bar_x + bar_width);
        layout.boundaries.sort_by(f32::total_cmp);
        layout.boundaries.dedup_by(|a, b| (*a - *b).abs() < 0.5);
    }
}

impl<Message> iced::advanced::widget::Meta for Timeline<'_, Message> {}

impl<Message> Widget<Message, Theme, Renderer> for Timeline<'_, Message> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, (BAR_HIT_RADIUS * 2.0).into())
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn layout(&mut self, tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) {
        tree.size = layout::atomic(limits, Length::Fill, BAR_HIT_RADIUS * 2.0);

        let (bar_x, bar_width) = local_bar_bounds(tree.size.width);
        let state = tree.state.downcast_mut::<State>();
        self.update_chapter_layout(&mut state.chapter_layout, bar_x, bar_width);
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout,
        cursor: Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if bounds.width < 1.0 || bounds.height < 1.0 {
            return;
        }

        let state = tree.state.downcast_ref::<State>();
        renderer.with_translation(Vector::new(bounds.x, bounds.y), |renderer| {
            let palette = theme.palette();
            let (bar_x, bar_width) = local_bar_bounds(bounds.width);

            let hovering_bar = cursor
                .position()
                .is_some_and(|position| is_over_bar(position, bounds));
            let active_bar = state.drag.is_some() || hovering_bar;
            let bar_height = if active_bar {
                ACTIVE_BAR_HEIGHT
            } else {
                BAR_HEIGHT
            };
            let bar_y = bounds.height - BAR_BOTTOM_PADDING - bar_height / 2.0;

            let percentage = if self.duration_ms > 0 {
                self.position_ms.clamp(0, self.duration_ms) as f32 / self.duration_ms as f32
            } else {
                0.0
            };
            let progress_x = bar_x + bar_width * percentage.clamp(0.0, 1.0);
            let hover_x = self.hover_x(state, bounds.width);

            let boundaries = &state.chapter_layout.boundaries;
            let hovered_segment = hover_x.map(|x| {
                boundaries
                    .partition_point(|boundary| *boundary <= x)
                    .saturating_sub(1)
                    .min(boundaries.len() - 2)
            });

            for (index, segment) in boundaries.windows(2).enumerate() {
                let segment_height = if hovered_segment == Some(index) {
                    HOVERED_SEGMENT_HEIGHT
                } else {
                    bar_height
                };
                let segment_y = bar_y + (bar_height - segment_height) / 2.0;
                let segment_width = segment[1] - segment[0];
                let start_gap = if index > 0 {
                    3.0_f32
                        .min(segment_width / 2.0)
                        .min((segment[0] - boundaries[index - 1]) / 2.0)
                } else {
                    0.0
                };
                let end_gap = if index + 2 < boundaries.len() {
                    3.0_f32
                        .min(segment_width / 2.0)
                        .min((boundaries[index + 2] - segment[1]) / 2.0)
                } else {
                    0.0
                };
                let start = segment[0] + start_gap / 2.0;
                let end = segment[1] - end_gap / 2.0;
                fill_segment(
                    renderer,
                    start,
                    end,
                    segment_y,
                    segment_height,
                    palette.background.base.text.scale_alpha(0.3),
                );
                fill_segment(
                    renderer,
                    start,
                    end.min(progress_x),
                    segment_y,
                    segment_height,
                    palette.primary.base.color,
                );
                if let Some(hover_x) = hover_x
                    && hover_x > progress_x
                {
                    fill_segment(
                        renderer,
                        start.max(progress_x),
                        end.min(hover_x),
                        segment_y,
                        segment_height,
                        palette.background.base.text.scale_alpha(0.5),
                    );
                }
            }

            let handle_radius = if active_bar {
                ACTIVE_HANDLE_RADIUS
            } else {
                HANDLE_RADIUS
            };
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        Point::new(
                            progress_x - handle_radius,
                            bar_y + bar_height / 2.0 - handle_radius,
                        ),
                        Size::new(handle_radius * 2.0, handle_radius * 2.0),
                    ),
                    border: border::rounded(handle_radius),
                    snap: false,
                    ..Default::default()
                },
                palette.background.base.text,
            );
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout,
        cursor: Cursor,
        _renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<State>();

        let previous_hover = state.hover_x;
        let previous_position = state.position_ms;
        let was_dragging = state.drag.is_some();

        let interactive = self.duration_ms > 0 && bounds.width > 0.0;

        state.hover_x = cursor
            .position()
            .filter(|position| self.duration_ms > 0 && is_over_bar(*position, bounds))
            .map(|position| position.x - bounds.x);

        if interactive && let Some(cursor_x) = self.drag_event(state, event, bounds, cursor, shell)
        {
            state.position_ms = self.time_at(cursor_x, bounds);

            if let Some(on_scrub) = &self.on_scrub {
                shell.publish(on_scrub(state.position_ms));
            }

            shell.capture_event();
            shell.request_redraw();
        }

        if previous_hover != state.hover_x
            || previous_position != state.position_ms
            || was_dragging != state.drag.is_some()
        {
            shell.invalidate_overlay();
            shell.request_redraw();
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout,
        cursor: Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let bounds = layout.bounds();
        if self.duration_ms > 0
            && cursor
                .position()
                .is_some_and(|position| is_over_bar(position, bounds))
        {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout,
        renderer: &Renderer,
        _viewport: &Rectangle,
        translation: Vector,
        window: Size,
    ) -> Vec<overlay::Element<'a, Message, Theme, Renderer>> {
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<State>();
        let Some(hover_x) = self.hover_x(state, bounds.width) else {
            return Vec::new();
        };
        let position_ms = self.time_at(bounds.x + hover_x, bounds);
        let title = self
            .chapters
            .iter()
            .rev()
            .find(|chapter| chapter.start_ms <= position_ms && position_ms < chapter.end_ms)
            .and_then(|chapter| chapter.title.as_deref())
            .filter(|title| !title.is_empty());

        let title = title.map(|title| {
            text(title)
                .size(13)
                .wrapping(renderer_text::Wrapping::None)
                .ellipsis(renderer_text::Ellipsis::End)
        });
        let time = text(format_time(position_ms)).size(14);

        let content = column![title, time].align_x(iced::alignment::Horizontal::Center);

        let mut content = container(content).padding([2, 10]).style(|theme: &Theme| {
            let palette = theme.palette();
            container::Style {
                background: Some(palette.background.strong.color.into()),
                text_color: Some(palette.background.strong.text),
                border: border::rounded(4),
                ..Default::default()
            }
        });

        let inset = 4.0_f32.min(window.width / 2.0);
        let limits = layout::Limits::new(
            Size::ZERO,
            Size::new((window.width - inset * 2.0).max(0.0), window.height),
        );

        let tree = state.hover_label.get_or_insert_with(Tree::empty);
        tree.diff::<Message, Theme, Renderer>(&mut content);
        Widget::<Message, Theme, Renderer>::layout(&mut content, tree, renderer, &limits);
        let size = tree.size;

        let anchor = layout.position()
            + translation
            + Vector::new(
                hover_x,
                bounds.height - BAR_BOTTOM_PADDING - ACTIVE_HANDLE_RADIUS,
            );
        let x = (anchor.x - size.width / 2.0)
            .clamp(inset, (window.width - size.width - inset).max(inset));
        let y = (anchor.y - size.height - 4.0).clamp(0.0, (window.height - size.height).max(0.0));

        vec![overlay::Element::new(Box::new(HoverLabel {
            content,
            tree,
            layout: Layout::new(size).move_to(Point::new(x, y)),
        }))]
    }
}

impl<Message, W> overlay::Overlay<Message, Theme, Renderer> for HoverLabel<'_, W>
where
    W: Widget<Message, Theme, Renderer>,
{
    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        cursor: Cursor,
    ) {
        renderer.with_layer(self.layout.bounds(), |renderer| {
            self.content.draw(
                self.tree,
                renderer,
                theme,
                style,
                self.layout,
                cursor,
                &self.layout.bounds(),
            );
        });
    }
}
