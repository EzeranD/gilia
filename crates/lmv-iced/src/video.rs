use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

use ash::vk::Handle;
use ffmpeg_next::format::Pixel;
use iced::{
    wgpu::{
        self, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
        BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, Buffer, BufferUsages,
        ColorTargetState, ColorWrites, Device, Extent3d, FragmentState, MultisampleState,
        Operations, PipelineCompilationOptions, PipelineLayoutDescriptor, PrimitiveState, Queue,
        RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline, RenderPipelineDescriptor,
        ShaderModuleDescriptor, ShaderStages, TexelCopyBufferLayout, Texture, TextureAspect,
        TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureUses,
        TextureView, TextureViewDescriptor, TextureViewDimension, VertexState,
        hal::vulkan::{self, TextureMemory},
        wgt::BufferDescriptor,
    },
    widget::shader,
};
use lmv_engine::{Frame, GpuFrame, VideoFrame};

use crate::widget::{VideoUniform, compute_scale};

#[derive(Debug)]
pub struct VideoPrimitive {
    pub frame: VideoFrame,
    pub current_pts: Arc<AtomicI64>,
}

impl VideoPrimitive {
    pub fn new(frame: VideoFrame, current_pts: Arc<AtomicI64>) -> Self {
        Self { frame, current_pts }
    }

    fn handle_format(
        &self,
        pipeline: &mut VideoPipeline,
        device: &Device,
        queue: &Queue,
        width: u32,
        height: u32,
    ) {
        if let Frame::Sw(frame) = &self.frame.frame {
            match frame.format() {
                Pixel::YUV420P => {
                    let (y, y_view) = upload_plane(
                        device,
                        queue,
                        Some("y plane"),
                        width,
                        height,
                        TextureFormat::R8Unorm,
                        Some(frame.data(0)),
                        Some(frame.stride(0) as u32),
                    );
                    let (u, u_view) = upload_plane(
                        device,
                        queue,
                        Some("u plane"),
                        width >> self.frame.info.log2_chroma_w,
                        height >> self.frame.info.log2_chroma_h,
                        TextureFormat::R8Unorm,
                        Some(frame.data(1)),
                        Some(frame.stride(1) as u32),
                    );
                    let (v, v_view) = upload_plane(
                        device,
                        queue,
                        Some("v plane"),
                        width >> self.frame.info.log2_chroma_w,
                        height >> self.frame.info.log2_chroma_h,
                        TextureFormat::R8Unorm,
                        Some(frame.data(2)),
                        Some(frame.stride(2) as u32),
                    );
                    pipeline.textures.push(y);
                    pipeline.textures.push(u);
                    pipeline.textures.push(v);
                    pipeline.texture_views.push(y_view);
                    pipeline.texture_views.push(u_view);
                    pipeline.texture_views.push(v_view);
                }
                Pixel::YUV420P10LE => {
                    let (y, y_view) = upload_plane(
                        device,
                        queue,
                        Some("y plane"),
                        width,
                        height,
                        TextureFormat::R16Unorm,
                        Some(frame.data(0)),
                        Some(frame.stride(0) as u32),
                    );
                    let (u, u_view) = upload_plane(
                        device,
                        queue,
                        Some("u plane"),
                        width >> self.frame.info.log2_chroma_w,
                        height >> self.frame.info.log2_chroma_h,
                        TextureFormat::R16Unorm,
                        Some(frame.data(1)),
                        Some(frame.stride(1) as u32),
                    );
                    let (v, v_view) = upload_plane(
                        device,
                        queue,
                        Some("v plane"),
                        width >> self.frame.info.log2_chroma_w,
                        height >> self.frame.info.log2_chroma_h,
                        TextureFormat::R16Unorm,
                        Some(frame.data(2)),
                        Some(frame.stride(2) as u32),
                    );
                    pipeline.textures.push(y);
                    pipeline.textures.push(u);
                    pipeline.textures.push(v);
                    pipeline.texture_views.push(y_view);
                    pipeline.texture_views.push(u_view);
                    pipeline.texture_views.push(v_view);
                }
                Pixel::NV12 => {
                    let (y, y_view) = upload_plane(
                        device,
                        queue,
                        Some("y plane"),
                        width,
                        height,
                        TextureFormat::R8Unorm,
                        Some(frame.data(0)),
                        Some(frame.stride(0) as u32),
                    );
                    let (uv, uv_view) = upload_plane(
                        device,
                        queue,
                        Some("u plane"),
                        width >> self.frame.info.log2_chroma_w,
                        height >> self.frame.info.log2_chroma_h,
                        TextureFormat::Rg8Unorm,
                        Some(frame.data(1)),
                        Some(frame.stride(1) as u32),
                    );
                    pipeline.textures.push(y);
                    pipeline.textures.push(uv);
                    pipeline.texture_views.push(y_view);
                    pipeline.texture_views.push(uv_view);
                }
                Pixel::P010LE => {
                    let (y, y_view) = upload_plane(
                        device,
                        queue,
                        Some("y plane"),
                        width,
                        height,
                        TextureFormat::R16Unorm,
                        Some(frame.data(0)),
                        Some(frame.stride(0) as u32),
                    );
                    let (uv, uv_view) = upload_plane(
                        device,
                        queue,
                        Some("u plane"),
                        width >> self.frame.info.log2_chroma_w,
                        height >> self.frame.info.log2_chroma_h,
                        TextureFormat::Rg16Unorm,
                        Some(frame.data(1)),
                        Some(frame.stride(1) as u32),
                    );
                    pipeline.textures.push(y);
                    pipeline.textures.push(uv);
                    pipeline.texture_views.push(y_view);
                    pipeline.texture_views.push(uv_view);
                }
                _ => panic!("Unsupported pixel format {:?}", frame.format()),
            }
        }
    }

    fn handle_gpu(&self, pipeline: &mut VideoPipeline, device: &Device, gpu_frame: &GpuFrame) {
        let hal_dev = unsafe { device.as_hal::<vulkan::Api>() }.unwrap();
        match gpu_frame {
            GpuFrame::Vaapi { planes } => {
                for (i, plane) in planes.iter().enumerate() {
                    let plane_width = if i == 0 {
                        self.frame.info.width
                    } else {
                        self.frame.info.width >> self.frame.info.log2_chroma_w
                    };
                    let plane_height = if i == 0 {
                        self.frame.info.height
                    } else {
                        self.frame.info.height >> self.frame.info.log2_chroma_h
                    };
                    let format = if i == 0 {
                        if self.frame.info.bit_depth >= 10 {
                            TextureFormat::R16Unorm
                        } else {
                            TextureFormat::R8Unorm
                        }
                    } else {
                        if self.frame.info.bit_depth >= 10 {
                            if self.frame.info.is_packed {
                                TextureFormat::Rg16Unorm
                            } else {
                                TextureFormat::R16Unorm
                            }
                        } else {
                            if self.frame.info.is_packed {
                                TextureFormat::Rg8Unorm
                            } else {
                                TextureFormat::R8Unorm
                            }
                        }
                    };
                    let hal_desc = wgpu::hal::TextureDescriptor {
                        label: None,
                        size: wgpu::Extent3d {
                            width: plane_width,
                            height: plane_height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUses::RESOURCE,
                        memory_flags: wgpu::hal::MemoryFlags::empty(),
                        view_formats: Vec::new(),
                    };

                    let hal_texture = unsafe {
                        hal_dev
                            .texture_from_dmabuf_fd(
                                plane.fd.try_clone().unwrap(),
                                &hal_desc,
                                plane.format_modifier,
                                plane.pitch as u64,
                                plane.offset as u64,
                            )
                            .unwrap()
                    };
                    let texture = unsafe {
                        device.create_texture_from_hal::<vulkan::Api>(
                            hal_texture,
                            &TextureDescriptor {
                                label: Some("temp texture label"),
                                size: Extent3d {
                                    width: plane_width,
                                    height: plane_height,
                                    depth_or_array_layers: 1,
                                },
                                mip_level_count: 1,
                                sample_count: 1,
                                dimension: TextureDimension::D2,
                                format,
                                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                                view_formats: &[],
                            },
                            TextureUses::RESOURCE,
                        )
                    };
                    let t_view = texture.create_view(&TextureViewDescriptor::default());
                    pipeline.textures.push(texture);
                    pipeline.texture_views.push(t_view);
                }
            }
            GpuFrame::Vulkan { images } => {
                for (i, image) in images.iter().enumerate() {
                    if image.is_null() {
                        continue;
                    }
                    let plane_width = if i == 0 {
                        self.frame.info.width
                    } else {
                        self.frame.info.width >> self.frame.info.log2_chroma_w
                    };
                    let plane_height = if i == 0 {
                        self.frame.info.height
                    } else {
                        self.frame.info.height >> self.frame.info.log2_chroma_h
                    };
                    let format = if self.frame.info.bit_depth >= 10 {
                        TextureFormat::P010
                    } else {
                        TextureFormat::NV12
                    };
                    let hal_desc = wgpu::hal::TextureDescriptor {
                        label: None,
                        size: wgpu::Extent3d {
                            width: plane_width,
                            height: plane_height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUses::RESOURCE,
                        memory_flags: wgpu::hal::MemoryFlags::empty(),
                        view_formats: Vec::new(),
                    };

                    let hal_texture = unsafe {
                        hal_dev.texture_from_raw(
                            *image,
                            &hal_desc,
                            Some(Box::new(|| {})),
                            TextureMemory::External,
                        )
                    };
                    let texture = unsafe {
                        device.create_texture_from_hal::<vulkan::Api>(
                            hal_texture,
                            &TextureDescriptor {
                                label: Some("temp texture label"),
                                size: Extent3d {
                                    width: plane_width,
                                    height: plane_height,
                                    depth_or_array_layers: 1,
                                },
                                mip_level_count: 1,
                                sample_count: 1,
                                dimension: TextureDimension::D2,
                                format,
                                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                                view_formats: &[],
                            },
                            TextureUses::RESOURCE,
                        )
                    };
                    let is_10bit = self.frame.info.bit_depth > 8;
                    let y_view = texture.create_view(&TextureViewDescriptor {
                        label: Some("y plane"),
                        format: Some(if is_10bit {
                            TextureFormat::R16Unorm
                        } else {
                            TextureFormat::R8Unorm
                        }),
                        dimension: Some(TextureViewDimension::D2),
                        usage: Some(TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST),
                        aspect: TextureAspect::Plane0,
                        base_mip_level: 0,
                        mip_level_count: Some(1),
                        base_array_layer: 0,
                        array_layer_count: Some(1),
                    });
                    let uv_view = texture.create_view(&TextureViewDescriptor {
                        label: Some("uv plane"),
                        format: Some(if is_10bit {
                            TextureFormat::Rg16Unorm
                        } else {
                            TextureFormat::Rg8Unorm
                        }),
                        dimension: Some(TextureViewDimension::D2),
                        usage: Some(TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST),
                        aspect: TextureAspect::Plane1,
                        base_mip_level: 0,
                        mip_level_count: Some(1),
                        base_array_layer: 0,
                        array_layer_count: Some(1),
                    });
                    pipeline.textures.push(texture);
                    pipeline.texture_views.push(y_view);
                    pipeline.texture_views.push(uv_view);
                }
            }
        }
    }
}

pub fn upload_plane(
    device: &iced::wgpu::Device,
    queue: &iced::wgpu::Queue,
    label: Option<&str>,
    width: u32,
    height: u32,
    format: TextureFormat,
    data: Option<&[u8]>,
    stride: Option<u32>,
) -> (Texture, TextureView) {
    let texture = device.create_texture(&TextureDescriptor {
        label,
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
        view_formats: &[],
    });
    if let Some(data) = data {
        queue.write_texture(
            texture.as_image_copy(),
            data,
            TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: stride,
                rows_per_image: None,
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }
    let texture_view = texture.create_view(&TextureViewDescriptor::default());
    (texture, texture_view)
}

impl shader::Primitive for VideoPrimitive {
    type Pipeline = VideoPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &iced::wgpu::Device,
        queue: &iced::wgpu::Queue,
        bounds: &iced::Rectangle,
        _viewport: &shader::Viewport,
    ) {
        let current_pts = self.current_pts.load(Ordering::Relaxed);
        let resolution_changed =
            pipeline.width != self.frame.info.width || pipeline.height != self.frame.info.height;
        let bounds_changed = pipeline.bound_width != bounds.width as u32
            || pipeline.bound_height != bounds.height as u32;
        if pipeline.last_pts == Some(current_pts) && !resolution_changed && !bounds_changed {
            return;
        }
        pipeline.last_pts = Some(current_pts);

        if resolution_changed {
            pipeline.width = self.frame.info.width;
            pipeline.height = self.frame.info.height;
            pipeline.texture_views.clear();
            pipeline.textures.clear();
            let width = self.frame.info.width;
            let height = self.frame.info.height;
            match &self.frame.frame {
                Frame::Hw(frame, _) => {
                    self.handle_gpu(pipeline, device, frame);
                    create_bind_group(pipeline, device);
                }
                Frame::Sw(_frame) => {
                    self.handle_format(pipeline, device, queue, width, height);
                    create_bind_group(pipeline, device);
                }
            }
        } else {
            match &self.frame.frame {
                Frame::Hw(frame, _) => {
                    pipeline.texture_views.clear();
                    pipeline.textures.clear();
                    self.handle_gpu(pipeline, device, frame);
                    create_bind_group(pipeline, device);
                }
                Frame::Sw(frame) => {
                    for i in 0..pipeline.textures.len() {
                        queue.write_texture(
                            pipeline.textures[i].as_image_copy(),
                            frame.data(i),
                            TexelCopyBufferLayout {
                                offset: 0,
                                bytes_per_row: Some(frame.stride(i) as u32),
                                rows_per_image: None,
                            },
                            pipeline.textures[i].size(),
                        );
                    }
                }
            }
        }
        if bounds_changed || resolution_changed {
            pipeline.bound_width = bounds.width as u32;
            pipeline.bound_height = bounds.height as u32;
            let is_packed = if self.frame.info.is_packed { 1 } else { 0 };
            let scale = compute_scale(
                bounds,
                self.frame.info.width as f32,
                self.frame.info.height as f32,
            );
            let video_uniform = VideoUniform::new(
                is_packed,
                scale,
                self.frame.info.color_matrix,
                self.frame.info.multiplier,
            );
            queue.write_buffer(
                &pipeline.video_uniform,
                0,
                bytemuck::bytes_of(&video_uniform),
            );
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
            label: Some("video render pass"),
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

fn create_bind_group(pipeline: &mut VideoPipeline, device: &Device) {
    let bind_group = device.create_bind_group(&BindGroupDescriptor {
        label: Some("video bind group"),
        layout: &pipeline.bind_group_layout,
        entries: &[
            BindGroupEntry {
                binding: 0,
                resource: BindingResource::TextureView(&pipeline.texture_views[0]),
            },
            BindGroupEntry {
                binding: 1,
                resource: BindingResource::TextureView(&pipeline.texture_views[1]),
            },
            BindGroupEntry {
                binding: 2,
                resource: BindingResource::TextureView(
                    &pipeline.texture_views[if pipeline.texture_views.len() > 2 {
                        2
                    } else {
                        1
                    }],
                ),
            },
            BindGroupEntry {
                binding: 3,
                resource: pipeline.video_uniform.as_entire_binding(),
            },
        ],
    });
    pipeline.bind_group = Some(bind_group);
}

pub struct VideoPipeline {
    texture_views: Vec<TextureView>,
    video_uniform: Buffer,
    bind_group_layout: BindGroupLayout,
    bind_group: Option<BindGroup>,
    pipeline: RenderPipeline,
    textures: Vec<Texture>,
    width: u32,
    height: u32,
    bound_width: u32,
    bound_height: u32,
    last_pts: Option<i64>,
}

impl VideoPipeline {
    fn new(device: &Device, _queue: &Queue, format: TextureFormat) -> Self {
        let texture_views = Vec::new();
        let video_uniform = device.create_buffer(&BufferDescriptor {
            label: Some("video uniform buffer"),
            size: std::mem::size_of::<VideoUniform>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("layout"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: iced::wgpu::BindingType::Texture {
                        sample_type: iced::wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: iced::wgpu::BindingType::Texture {
                        sample_type: iced::wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::FRAGMENT,
                    ty: iced::wgpu::BindingType::Texture {
                        sample_type: iced::wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 3,
                    visibility: ShaderStages::VERTEX_FRAGMENT,
                    ty: iced::wgpu::BindingType::Buffer {
                        ty: iced::wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("video pipline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("video shader"),
            source: iced::wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "../../../shaders/video.wgsl"
            ))),
        });
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("video render pipeline"),
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
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self {
            texture_views,
            video_uniform,
            bind_group_layout,
            bind_group: None,
            pipeline,
            textures: Vec::new(),
            width: 0,
            height: 0,
            bound_width: 0,
            bound_height: 0,
            last_pts: None,
        }
    }
}

impl shader::Pipeline for VideoPipeline {
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
