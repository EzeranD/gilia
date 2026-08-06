// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gilia_engine::Session;
use iced::{
    Event, Length, Rectangle, Size,
    advanced::{
        Layout, Shell, layout, mouse, renderer,
        widget::{Meta, Tree},
    },
};

use crate::{subs::SubPrimitive, video::VideoPrimitive};

pub struct PlayerWidget<'a> {
    session: &'a Session,
    width: Length,
    height: Length,
}

impl<'a> PlayerWidget<'a> {
    pub fn new(session: &'a Session) -> Self {
        Self {
            session,
            width: Length::Fill,
            height: Length::Fill,
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

impl Meta for PlayerWidget<'_> {}

impl<Message, Theme, Renderer> iced::advanced::Widget<Message, Theme, Renderer> for PlayerWidget<'_>
where
    Renderer: iced_wgpu::primitive::Renderer,
{
    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(&mut self, tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) {
        tree.size = layout::atomic(limits, self.width, self.height);
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        _event: &Event,
        _layout: Layout,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        if let Some(frame) = self.session.frame() {
            let frame_pts = frame.info.pts.unwrap();
            let width = frame.info.width;
            let height = frame.info.height;
            renderer.draw_primitive(
                layout.bounds(),
                VideoPrimitive::new(frame, self.session.video_output.current_pts.clone()),
            );
            let phys_width =
                layout.bounds().width * renderer.scale().map_or(1.0, renderer::Scale::total);
            let phys_height =
                layout.bounds().height * renderer.scale().map_or(1.0, renderer::Scale::total);
            self.session.update_viewport(
                phys_width,
                phys_height,
                compute_scale(&layout.bounds(), width as f32, height as f32),
            );
            if let Some(subs) = self.session.current_subs(frame_pts)
                && !subs.layers.is_empty()
            {
                renderer.draw_primitive(
                    layout.bounds(),
                    SubPrimitive::new(subs, phys_width.round() as u32, phys_height.round() as u32),
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
