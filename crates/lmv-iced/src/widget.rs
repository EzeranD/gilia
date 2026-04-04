use std::time::{Duration, Instant};

use iced::{
    Element, Event, Length, Rectangle, Size,
    advanced::{Layout, Shell, Widget, layout, mouse, renderer, widget::Tree},
};
use lmv_engine::{PlaybackMode, PlaybackPhase, PlayerEngine};

use crate::{subs::SubPrimitive, video::VideoPrimitive};

pub struct PlayerWidget<'a> {
    engine: &'a PlayerEngine,
    width: Length,
    height: Length,
    pending_tick: bool,
    next_tick: u64,
}

impl<'a> PlayerWidget<'a> {
    pub fn new(engine: &'a PlayerEngine) -> Self {
        Self {
            engine,
            width: Length::Fill,
            height: Length::Fill,
            pending_tick: true,
            next_tick: 10,
        }
    }

    #[must_use]
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    #[must_use]
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }
}

impl<'a, Message, Theme, Renderer> From<PlayerWidget<'a>> for Element<'a, Message, Theme, Renderer>
where
    Renderer: iced_wgpu::primitive::Renderer,
{
    fn from(widget: PlayerWidget<'a>) -> Self {
        Element::new(widget)
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for PlayerWidget<'_>
where
    Renderer: iced_wgpu::primitive::Renderer,
{
    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, self.width, self.height)
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = self.engine.state.load();
        let playing = matches!(state.mode, PlaybackMode::Playing)
            || matches!(state.phase, PlaybackPhase::Seeking(_));
        let redraw_requested = matches!(
            event,
            Event::Window(iced::window::Event::RedrawRequested(_))
        );

        if !playing {
            self.pending_tick = false;
            return;
        }

        if playing && !self.pending_tick {
            self.pending_tick = true;
            shell.request_redraw_at(Instant::now() + Duration::from_millis(1));
            return;
        }

        if redraw_requested && self.pending_tick {
            self.pending_tick = false;
            let (_new_frame, next_tick_ms) = self.engine.tick_playback();
            self.next_tick = next_tick_ms;
            let next_redraw = Instant::now() + Duration::from_millis(next_tick_ms);
            shell.request_redraw_at(next_redraw);
            self.pending_tick = true;
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        if let Some(frame) = self.engine.frame() {
            let frame_pts = frame.info.pts.unwrap();
            let width = frame.info.width;
            let height = frame.info.height;
            renderer.draw_primitive(layout.bounds(), VideoPrimitive::new(frame));
            self.engine.update_viewport(
                layout.bounds().width,
                layout.bounds().height,
                compute_scale(&layout.bounds(), width as f32, height as f32),
            );
            if let Some(subs) = self.engine.current_subs(frame_pts)
                && !subs.layers.is_empty()
            {
                renderer.draw_primitive(
                    layout.bounds(),
                    SubPrimitive::new(
                        subs,
                        layout.bounds().width as u32,
                        layout.bounds().height as u32,
                    ),
                );
            }
        }
    }
}

#[must_use]
pub fn compute_scale(bounds: &iced::Rectangle, frame_width: f32, frame_height: f32) -> [f32; 2] {
    let bounds_ratio = bounds.width / bounds.height;
    let frame_ratio = frame_width / frame_height;
    let ratio = bounds_ratio / frame_ratio;
    // debug!("ratio: {ratio}");
    if ratio < 1.0 {
        [1.0, ratio]
    } else {
        [1.0 / ratio, 1.0]
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct VideoUniform {
    multiplier: f32,
    is_packed: u32,
    scale: [f32; 2],
    coeffs: [[f32; 4]; 3],
}

impl VideoUniform {
    #[must_use]
    pub fn new(is_packed: u32, scale: [f32; 2], coeffs: [[f32; 4]; 3], multiplier: f32) -> Self {
        Self {
            multiplier,
            is_packed,
            scale,
            coeffs,
        }
    }
}
