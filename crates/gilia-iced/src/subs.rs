// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gilia_engine::SubtitleFrame;
use iced::{
    wgpu::{
        BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
        BindGroupLayoutEntry, BlendState, ColorTargetState, ColorWrites, Device, FragmentState,
        MultisampleState, Operations, PipelineCompilationOptions, PipelineLayoutDescriptor,
        PrimitiveState, Queue, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
        RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderStages, TexelCopyBufferLayout,
        Texture, TextureFormat, TextureView, VertexState,
    },
    widget::shader,
};
use libass::Change;

use crate::video::upload_plane;

#[derive(Debug)]
pub struct SubPrimitive {
    pub subtitle: SubtitleFrame,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub struct SubPipeline {
    textures: Vec<Texture>,
    texture_views: Vec<TextureView>,
    bind_group_layout: BindGroupLayout,
    bind_group: Option<BindGroup>,
    pipeline: RenderPipeline,
    bound_width: u32,
    bound_height: u32,
    sub_data: Vec<u8>,
}

impl SubPrimitive {
    pub fn new(subtitle: SubtitleFrame, width: u32, height: u32) -> Self {
        Self {
            subtitle,
            width,
            height,
        }
    }
}

impl shader::Primitive for SubPrimitive {
    type Pipeline = SubPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &iced::wgpu::Device,
        queue: &iced::wgpu::Queue,
        bounds: &iced::Rectangle,
        _viewport: &shader::Viewport,
    ) {
        let bounds_changed = pipeline.bound_width != bounds.width as u32
            || pipeline.bound_height != bounds.height as u32;
        if bounds_changed {
            pipeline.bound_width = bounds.width as u32;
            pipeline.bound_height = bounds.height as u32;
            pipeline.texture_views.clear();
            pipeline.textures.clear();
            let (sub, sub_view) = upload_plane(
                device,
                queue,
                Some("subtitle"),
                self.width,
                self.height,
                TextureFormat::Rgba8Uint,
                None,
                Some(self.width * 4),
            );
            pipeline.textures.push(sub);
            pipeline.texture_views.push(sub_view);
            let bind_group = device.create_bind_group(&BindGroupDescriptor {
                label: Some("subs bing group"),
                layout: &pipeline.bind_group_layout,
                entries: &[BindGroupEntry {
                    binding: 0,
                    resource: iced::wgpu::BindingResource::TextureView(&pipeline.texture_views[0]),
                }],
            });
            pipeline.bind_group = Some(bind_group);
            pipeline
                .sub_data
                .resize((self.width * self.height * 4) as usize, 0);
        }
        let content_change = self.subtitle.change != Change::None;

        if bounds_changed || content_change {
            pipeline.sub_data.fill(0);
            for layer in &self.subtitle.layers {
                let r = (layer.color >> 24) as u8;
                let g = (layer.color >> 16) as u8;
                let b = (layer.color >> 8) as u8;
                let a = 255 - (layer.color & 0xFF);

                for (i, &alpha) in layer.bitmap.iter().enumerate() {
                    if alpha == 0 {
                        continue;
                    }
                    let real_alpha = (a * alpha as u32) / 255;
                    let row = i as i32 / layer.width;
                    let col = i as i32 % layer.width;
                    let dest_x = layer.x + col;
                    let dest_y = layer.y + row;
                    if dest_x >= 0
                        && dest_x < self.width as i32
                        && dest_y >= 0
                        && dest_y < self.height as i32
                    {
                        let idx = ((dest_y as u32 * self.width + dest_x as u32) * 4) as usize;
                        pipeline.sub_data[idx] = ((r as u32 * real_alpha
                            + pipeline.sub_data[idx] as u32 * (255 - real_alpha))
                            / 255) as u8;
                        pipeline.sub_data[idx + 1] = ((g as u32 * real_alpha
                            + pipeline.sub_data[idx + 1] as u32 * (255 - real_alpha))
                            / 255) as u8;
                        pipeline.sub_data[idx + 2] = ((b as u32 * real_alpha
                            + pipeline.sub_data[idx + 2] as u32 * (255 - real_alpha))
                            / 255) as u8;
                        pipeline.sub_data[idx + 3] = ((real_alpha * 255
                            + pipeline.sub_data[idx + 3] as u32 * (255 - real_alpha))
                            / 255) as u8;
                    }
                }
            }
            for i in 0..pipeline.textures.len() {
                queue.write_texture(
                    pipeline.textures[i].as_image_copy(),
                    &pipeline.sub_data,
                    TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(self.width * 4),
                        rows_per_image: None,
                    },
                    pipeline.textures[i].size(),
                );
            }
        }
    }

    fn draw(
        &self,
        _pipeline: &Self::Pipeline,
        _render_pass: &mut iced::wgpu::RenderPass<'_>,
    ) -> bool {
        false
    }

    fn render(
        &self,
        pipeline: &Self::Pipeline,
        encoder: &mut iced::wgpu::CommandEncoder,
        target: &iced::wgpu::TextureView,
        clip_bounds: &iced::Rectangle<u32>,
    ) {
        let Some(bind_group) = &pipeline.bind_group else {
            return;
        };
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("subs render pass"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: iced::wgpu::LoadOp::Load,
                    store: iced::wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_viewport(
            clip_bounds.x as f32,
            clip_bounds.y as f32,
            clip_bounds.width as f32,
            clip_bounds.height as f32,
            0.0,
            1.0,
        );
        pass.set_bind_group(0, bind_group, &[]);
        pass.set_pipeline(&pipeline.pipeline);
        pass.draw(0..6, 0..1);
    }
}

impl SubPipeline {
    fn new(device: &Device, _queue: &Queue, format: TextureFormat) -> Self {
        let texture_views = Vec::new();
        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("sub bind layout"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::FRAGMENT,
                ty: iced::wgpu::BindingType::Texture {
                    sample_type: iced::wgpu::TextureSampleType::Uint,
                    view_dimension: iced::wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("subs pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("subs shader"),
            source: iced::wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "../../../shaders/subs.wgsl"
            ))),
        });
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("subs render pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: PrimitiveState::default(),
            depth_stencil: None,
            multisample: MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: PipelineCompilationOptions::default(),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self {
            textures: Vec::new(),
            texture_views,
            bind_group_layout,
            bind_group: None,
            pipeline,
            bound_width: 0,
            bound_height: 0,
            sub_data: Vec::new(),
        }
    }
}

impl shader::Pipeline for SubPipeline {
    fn new(
        device: &iced::wgpu::Device,
        queue: &iced::wgpu::Queue,
        format: iced::wgpu::TextureFormat,
    ) -> Self
    where
        Self: Sized,
    {
        Self::new(device, queue, format)
    }
}
