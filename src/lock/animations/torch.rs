use std::error::Error;

use crate::modules::{RenderContext, RenderSize};

const SHADER: &str = include_str!("torch.wgsl");
const ACCUMULATION_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const CACHE_LAYERS: u32 = 4;
const MAX_CACHE_UPDATES: u32 = 512;
const UNIFORM_BYTES: u64 = 80;
const PRESENT_UNIFORM_BYTES: u64 = 16;
const BOX_STRIDE: usize = 80;
const BOX_COUNT: usize = 18;

const REDSTONE_SLOT: u8 = 0;
const COPPER_SLOT: u8 = 1;
const SOUL_SLOT: u8 = 2;
const TORCH_SLOT: u8 = 3;

const OWNER_MASK: u32 = 7;
const DIRECT_LIGHT_FLAG: u32 = 1 << 3;
const REDSTONE_ON_ONLY_FLAG: u32 = 1 << 4;
const REDSTONE_OFF_ONLY_FLAG: u32 = 1 << 5;
const REDSTONE_SHEET_FLAG: u32 = 1 << 6;

pub struct TorchAnimation {
    format: Option<wgpu::TextureFormat>,
    trace_pipeline: Option<wgpu::RenderPipeline>,
    present_pipeline: Option<wgpu::RenderPipeline>,
    trace_bind_group: Option<wgpu::BindGroup>,
    present_layout: Option<wgpu::BindGroupLayout>,
    trace_uniforms: Option<wgpu::Buffer>,
    present_uniforms: Option<wgpu::Buffer>,
    boxes: Option<wgpu::Buffer>,
    accumulation: Option<AccumulationTarget>,
    temporal: TemporalAccumulator,
    uploaded_mask: Option<u8>,
}

struct AccumulationTarget {
    _texture: wgpu::Texture,
    layer_views: [wgpu::TextureView; CACHE_LAYERS as usize],
    present_bind_group: wgpu::BindGroup,
}

impl TorchAnimation {
    pub fn new() -> Self {
        Self {
            format: None,
            trace_pipeline: None,
            present_pipeline: None,
            trace_bind_group: None,
            present_layout: None,
            trace_uniforms: None,
            present_uniforms: None,
            boxes: None,
            accumulation: None,
            temporal: TemporalAccumulator::default(),
            uploaded_mask: None,
        }
    }

    pub fn ensure_initialized(
        &mut self,
        context: &RenderContext<'_>,
        size: RenderSize,
    ) -> Result<(), Box<dyn Error>> {
        if self.format != Some(context.surface_format) {
            self.initialize_static(context)?;
        }
        if self.temporal.resize(size) {
            self.rebuild_accumulation(context, size);
        }
        Ok(())
    }

    pub fn wants_continuous_frames(&self) -> bool {
        self.temporal.needs_update()
    }

    pub fn draw(
        &mut self,
        context: &RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        size: RenderSize,
        mask: u8,
    ) {
        self.ensure_initialized(context, size)
            .expect("torch animation initialized during surface configure");

        if self.uploaded_mask != Some(mask) {
            context.queue.write_buffer(
                self.present_uniforms
                    .as_ref()
                    .expect("torch present uniform buffer initialized"),
                0,
                &encoded_present_uniforms(mask),
            );
            self.uploaded_mask = Some(mask);
        }

        if let Some(update) = self.temporal.next_update() {
            context.queue.write_buffer(
                self.trace_uniforms
                    .as_ref()
                    .expect("torch trace uniform buffer initialized"),
                0,
                &encoded_trace_uniforms(size, update.sample_index),
            );
            let accumulation = self
                .accumulation
                .as_ref()
                .expect("torch accumulation target initialized");
            let color_attachments = accumulation
                .layer_views
                .iter()
                .map(|view| {
                    Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: if update.sample_index == 0 {
                                wgpu::LoadOp::Clear(wgpu::Color::BLACK)
                            } else {
                                wgpu::LoadOp::Load
                            },
                            store: wgpu::StoreOp::Store,
                        },
                    })
                })
                .collect::<Vec<_>>();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("torch four-component HDR accumulation"),
                color_attachments: &color_attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(
                self.trace_pipeline
                    .as_ref()
                    .expect("torch trace pipeline initialized"),
            );
            pass.set_bind_group(
                0,
                self.trace_bind_group
                    .as_ref()
                    .expect("torch trace bind group initialized"),
                &[],
            );
            pass.set_blend_constant(wgpu::Color {
                r: f64::from(update.new_sample_weight),
                g: f64::from(update.new_sample_weight),
                b: f64::from(update.new_sample_weight),
                a: f64::from(update.new_sample_weight),
            });
            pass.draw(0..3, 0..1);
        }

        let accumulation = self
            .accumulation
            .as_ref()
            .expect("torch accumulation target initialized");
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("torch contribution composite, tone-map, and present"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(
            self.present_pipeline
                .as_ref()
                .expect("torch present pipeline initialized"),
        );
        pass.set_bind_group(0, &accumulation.present_bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    fn initialize_static(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let mut images = crate::resources::load_torch_textures()?;
        if images.iter().any(|image| image.dimensions() != (16, 16)) {
            return Err("all torch animation textures must be 16x16 PNGs".into());
        }
        for image in &mut images {
            image::imageops::flip_vertical_in_place(image);
        }
        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("torch material texture array"),
            size: wgpu::Extent3d {
                width: 16,
                height: 16,
                depth_or_array_layers: images.len() as u32,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (layer, image) in images.iter().enumerate() {
            context.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: layer as u32,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                image.as_raw(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(16 * 4),
                    rows_per_image: Some(16),
                },
                wgpu::Extent3d {
                    width: 16,
                    height: 16,
                    depth_or_array_layers: 1,
                },
            );
        }
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("torch material texture array view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let trace_uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("torch camera and cache sample uniforms"),
            size: UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let present_uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("torch contribution composite mask"),
            size: PRESENT_UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let boxes = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("torch two-bank scene box table"),
            size: (BOX_COUNT * BOX_STRIDE) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        context.queue.write_buffer(&boxes, 0, &encoded_boxes());

        let trace_layout =
            context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("torch trace bind group layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                multisampled: false,
                                view_dimension: wgpu::TextureViewDimension::D2Array,
                                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Storage { read_only: true },
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ],
                });
        let present_layout =
            context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("torch present bind group layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 3,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                multisampled: false,
                                view_dimension: wgpu::TextureViewDimension::D2Array,
                                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 4,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ],
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("torch four-component ray tracing shader"),
                source: wgpu::ShaderSource::Wgsl(SHADER.into()),
            });
        let trace_pipeline_layout =
            context
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("torch trace pipeline layout"),
                    bind_group_layouts: &[Some(&trace_layout)],
                    immediate_size: 0,
                });
        let present_pipeline_layout =
            context
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("torch present pipeline layout"),
                    bind_group_layouts: &[Some(&present_layout)],
                    immediate_size: 0,
                });
        let blend = Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Constant,
                dst_factor: wgpu::BlendFactor::OneMinusConstant,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Constant,
                dst_factor: wgpu::BlendFactor::OneMinusConstant,
                operation: wgpu::BlendOperation::Add,
            },
        });
        let trace_targets: [Option<wgpu::ColorTargetState>; CACHE_LAYERS as usize] =
            std::array::from_fn(|_| {
                Some(wgpu::ColorTargetState {
                    format: ACCUMULATION_FORMAT,
                    blend,
                    write_mask: wgpu::ColorWrites::ALL,
                })
            });
        let trace_pipeline =
            context
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("torch four-component HDR trace pipeline"),
                    layout: Some(&trace_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("fs_accumulate"),
                        compilation_options: Default::default(),
                        targets: &trace_targets,
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                });
        let present_pipeline =
            context
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("torch contribution composite and tone-map pipeline"),
                    layout: Some(&present_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(if context.surface_format.is_srgb() {
                            "fs_present_srgb"
                        } else {
                            "fs_present_unorm"
                        }),
                        compilation_options: Default::default(),
                        targets: &[Some(context.surface_format.into())],
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                });
        let trace_bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("torch trace bind group"),
                layout: &trace_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: trace_uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: boxes.as_entire_binding(),
                    },
                ],
            });

        self.format = Some(context.surface_format);
        self.trace_pipeline = Some(trace_pipeline);
        self.present_pipeline = Some(present_pipeline);
        self.trace_bind_group = Some(trace_bind_group);
        self.present_layout = Some(present_layout);
        self.trace_uniforms = Some(trace_uniforms);
        self.present_uniforms = Some(present_uniforms);
        self.boxes = Some(boxes);
        self.accumulation = None;
        self.temporal.clear();
        self.uploaded_mask = None;
        Ok(())
    }

    fn rebuild_accumulation(&mut self, context: &RenderContext<'_>, size: RenderSize) {
        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("torch four-layer linear HDR contribution cache"),
            size: wgpu::Extent3d {
                width: size.width.max(1),
                height: size.height.max(1),
                depth_or_array_layers: CACHE_LAYERS,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ACCUMULATION_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let layer_views = std::array::from_fn(|layer| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("torch HDR contribution cache layer"),
                dimension: Some(wgpu::TextureViewDimension::D2),
                base_array_layer: layer as u32,
                array_layer_count: Some(1),
                ..Default::default()
            })
        });
        let array_view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("torch HDR contribution cache array view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            base_array_layer: 0,
            array_layer_count: Some(CACHE_LAYERS),
            ..Default::default()
        });
        let present_bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("torch contribution cache present bind group"),
                layout: self
                    .present_layout
                    .as_ref()
                    .expect("torch present layout initialized"),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&array_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: self
                            .present_uniforms
                            .as_ref()
                            .expect("torch present uniforms initialized")
                            .as_entire_binding(),
                    },
                ],
            });
        self.accumulation = Some(AccumulationTarget {
            _texture: texture,
            layer_views,
            present_bind_group,
        });
        self.uploaded_mask = None;
    }
}

impl Default for TorchAnimation {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TemporalUpdate {
    sample_index: u32,
    new_sample_weight: f32,
}

#[derive(Default)]
struct TemporalAccumulator {
    size: Option<RenderSize>,
    updates: u32,
}

impl TemporalAccumulator {
    fn resize(&mut self, size: RenderSize) -> bool {
        if self.size == Some(size) {
            return false;
        }
        self.size = Some(size);
        self.updates = 0;
        true
    }

    fn clear(&mut self) {
        self.size = None;
        self.updates = 0;
    }

    fn needs_update(&self) -> bool {
        self.size.is_some() && self.updates < MAX_CACHE_UPDATES
    }

    fn next_update(&mut self) -> Option<TemporalUpdate> {
        if self.updates >= MAX_CACHE_UPDATES {
            return None;
        }
        let update = TemporalUpdate {
            sample_index: self.updates,
            new_sample_weight: 1.0 / (self.updates + 1) as f32,
        };
        self.updates += 1;
        Some(update)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeometryBank {
    Shared,
    RedstoneOn,
    RedstoneOff,
}

#[derive(Clone, Copy)]
struct BoxData {
    min: [f32; 3],
    max: [f32; 3],
    light_level: f32,
    smoothness: f32,
    texture_index: u32,
    texture_offset_positive: [f32; 3],
    texture_offset_negative: [f32; 3],
    light_owner: Option<u8>,
    direct_light: bool,
    geometry_bank: GeometryBank,
    redstone_sheet: bool,
}

impl BoxData {
    fn flags(self) -> u32 {
        let mut flags = self.light_owner.map_or(0, |owner| u32::from(owner) + 1) & OWNER_MASK;
        if self.direct_light {
            flags |= DIRECT_LIGHT_FLAG;
        }
        flags |= match self.geometry_bank {
            GeometryBank::Shared => 0,
            GeometryBank::RedstoneOn => REDSTONE_ON_ONLY_FLAG,
            GeometryBank::RedstoneOff => REDSTONE_OFF_ONLY_FLAG,
        };
        if self.redstone_sheet {
            flags |= REDSTONE_SHEET_FLAG;
        }
        flags
    }
}

fn encoded_boxes() -> Vec<u8> {
    let mut bytes = vec![0; BOX_COUNT * BOX_STRIDE];
    for (index, box_data) in scene_boxes().iter().copied().enumerate() {
        let base = index * BOX_STRIDE;
        write_vec3(&mut bytes, base, box_data.min);
        write_vec3(&mut bytes, base + 16, box_data.max);
        write_f32(&mut bytes, base + 32, box_data.light_level);
        write_f32(&mut bytes, base + 36, box_data.smoothness);
        write_u32(&mut bytes, base + 40, box_data.texture_index);
        write_u32(&mut bytes, base + 44, box_data.flags());
        write_vec3(&mut bytes, base + 48, box_data.texture_offset_positive);
        write_vec3(&mut bytes, base + 64, box_data.texture_offset_negative);
    }
    bytes
}

fn encoded_trace_uniforms(size: RenderSize, sample_index: u32) -> [u8; UNIFORM_BYTES as usize] {
    let mut bytes = [0; UNIFORM_BYTES as usize];
    write_vec3(&mut bytes, 0, [1.0, 0.0, 0.0]);
    write_vec3(&mut bytes, 16, [0.0, 0.0, 1.0]);
    write_f32(&mut bytes, 32, 70.0_f32.to_radians());
    write_vec3(&mut bytes, 48, [0.0, -3.0, 1.0]);
    write_f32(&mut bytes, 64, size.width as f32);
    write_f32(&mut bytes, 68, size.height as f32);
    write_u32(&mut bytes, 72, sample_index);
    bytes
}

fn encoded_present_uniforms(mask: u8) -> [u8; PRESENT_UNIFORM_BYTES as usize] {
    let mut bytes = [0; PRESENT_UNIFORM_BYTES as usize];
    write_u32(&mut bytes, 0, u32::from(mask));
    bytes
}

fn write_vec3(bytes: &mut [u8], offset: usize, values: [f32; 3]) {
    for (index, value) in values.into_iter().enumerate() {
        write_f32(bytes, offset + index * 4, value);
    }
}

fn write_f32(bytes: &mut [u8], offset: usize, value: f32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn scene_boxes() -> [BoxData; BOX_COUNT] {
    const TEX_POS: [f32; 3] = [0.0, 0.0, 0.0625];
    const TEX_NEG: [f32; 3] = [0.0, 0.0, -0.4375];
    const NO_OFFSET: [f32; 3] = [0.0; 3];
    let torch_box = |min,
                     max,
                     light_level,
                     texture_index,
                     light_owner,
                     direct_light,
                     geometry_bank,
                     redstone_sheet| BoxData {
        min,
        max,
        light_level,
        smoothness: 0.0,
        texture_index,
        texture_offset_positive: TEX_POS,
        texture_offset_negative: TEX_NEG,
        light_owner,
        direct_light,
        geometry_bank,
        redstone_sheet,
    };
    let ground = |min, max| BoxData {
        min,
        max,
        light_level: 0.0,
        smoothness: 0.2,
        texture_index: 4,
        texture_offset_positive: NO_OFFSET,
        texture_offset_negative: NO_OFFSET,
        light_owner: None,
        direct_light: false,
        geometry_bank: GeometryBank::Shared,
        redstone_sheet: false,
    };
    [
        // Redstone-on bank: the hand-authored stem, four visual emissive sheets, and sampled head.
        torch_box(
            [-0.5625, 0.4375, 0.0],
            [-0.4375, 0.5625, 0.4375],
            0.0,
            0,
            None,
            false,
            GeometryBank::RedstoneOn,
            false,
        ),
        torch_box(
            [-1.0, 0.4375, 0.0],
            [0.0, 0.4375, 1.0],
            7.0,
            0,
            Some(REDSTONE_SLOT),
            false,
            GeometryBank::RedstoneOn,
            true,
        ),
        torch_box(
            [-1.0, 0.5625, 0.0],
            [0.0, 0.5625, 1.0],
            7.0,
            0,
            Some(REDSTONE_SLOT),
            false,
            GeometryBank::RedstoneOn,
            true,
        ),
        torch_box(
            [-0.5625, 0.0, 0.0],
            [-0.5625, 1.0, 1.0],
            7.0,
            0,
            Some(REDSTONE_SLOT),
            false,
            GeometryBank::RedstoneOn,
            true,
        ),
        torch_box(
            [-0.4375, 0.0, 0.0],
            [-0.4375, 1.0, 1.0],
            7.0,
            0,
            Some(REDSTONE_SLOT),
            false,
            GeometryBank::RedstoneOn,
            true,
        ),
        torch_box(
            [-0.5625, 0.4375, 0.5],
            [-0.4375, 0.5625, 0.625],
            7.0,
            0,
            Some(REDSTONE_SLOT),
            true,
            GeometryBank::RedstoneOn,
            false,
        ),
        // Redstone-off bank: the Minecraft-style compact model and dedicated off texture.
        torch_box(
            [-0.5625, 0.4375, 0.0],
            [-0.4375, 0.5625, 0.5],
            0.0,
            5,
            None,
            false,
            GeometryBank::RedstoneOff,
            false,
        ),
        torch_box(
            [-0.5625, 0.4375, 0.5],
            [-0.4375, 0.5625, 0.625],
            0.0,
            5,
            None,
            false,
            GeometryBank::RedstoneOff,
            false,
        ),
        // The other three torches are shared by both redstone geometry banks.
        torch_box(
            [0.4375, 0.4375, 0.0],
            [0.5625, 0.5625, 0.5],
            0.0,
            1,
            None,
            false,
            GeometryBank::Shared,
            false,
        ),
        torch_box(
            [0.4375, 0.4375, 0.5],
            [0.5625, 0.5625, 0.625],
            14.0,
            1,
            Some(COPPER_SLOT),
            true,
            GeometryBank::Shared,
            false,
        ),
        torch_box(
            [-0.5625, -0.5625, 0.0],
            [-0.4375, -0.4375, 0.5],
            0.0,
            2,
            None,
            false,
            GeometryBank::Shared,
            false,
        ),
        torch_box(
            [-0.5625, -0.5625, 0.5],
            [-0.4375, -0.4375, 0.625],
            10.0,
            2,
            Some(SOUL_SLOT),
            true,
            GeometryBank::Shared,
            false,
        ),
        torch_box(
            [0.4375, -0.5625, 0.0],
            [0.5625, -0.4375, 0.5],
            0.0,
            3,
            None,
            false,
            GeometryBank::Shared,
            false,
        ),
        torch_box(
            [0.4375, -0.5625, 0.5],
            [0.5625, -0.4375, 0.625],
            14.0,
            3,
            Some(TORCH_SLOT),
            true,
            GeometryBank::Shared,
            false,
        ),
        ground([0.0, 0.0, 0.0], [1.0, 1.0, 0.0]),
        ground([-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ground([-1.0, -1.0, 0.0], [0.0, 0.0, 0.0]),
        ground([0.0, -1.0, 0.0], [1.0, 0.0, 0.0]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_shader_parses_and_validates() {
        let module = wgpu::naga::front::wgsl::parse_str(SHADER).expect("torch WGSL parses");
        wgpu::naga::valid::Validator::new(
            wgpu::naga::valid::ValidationFlags::all(),
            wgpu::naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("torch WGSL validates");
    }

    #[test]
    fn contribution_cache_accumulates_globally_until_resize() {
        let mut temporal = TemporalAccumulator::default();
        let size = RenderSize {
            width: 10,
            height: 8,
        };
        assert!(temporal.resize(size));
        assert_eq!(temporal.next_update().unwrap().sample_index, 0);
        assert_eq!(temporal.next_update().unwrap().sample_index, 1);

        // Mask/state changes are deliberately absent from the cache accumulator.
        assert!(!temporal.resize(size));
        assert_eq!(temporal.next_update().unwrap().sample_index, 2);
    }

    #[test]
    fn resize_resets_only_the_changed_outputs_cache() {
        let mut temporal = TemporalAccumulator::default();
        let first = RenderSize {
            width: 10,
            height: 8,
        };
        assert!(temporal.resize(first));
        temporal.next_update();
        assert!(!temporal.resize(first));
        assert_eq!(temporal.updates, 1);

        assert!(temporal.resize(RenderSize {
            width: 11,
            height: 8,
        }));
        assert_eq!(temporal.updates, 0);
    }

    #[test]
    fn continuous_frames_stop_after_512_cache_updates() {
        let mut temporal = TemporalAccumulator::default();
        temporal.resize(RenderSize {
            width: 1,
            height: 1,
        });
        for expected in 0..MAX_CACHE_UPDATES {
            assert!(temporal.needs_update());
            assert_eq!(temporal.next_update().unwrap().sample_index, expected);
        }
        assert!(!temporal.needs_update());
        assert_eq!(temporal.next_update(), None);
    }

    #[test]
    fn redstone_off_bank_is_diffuse_and_uses_the_minecraft_texture() {
        for box_data in &scene_boxes()[6..8] {
            assert_eq!(box_data.geometry_bank, GeometryBank::RedstoneOff);
            assert_eq!(box_data.light_level, 0.0);
            assert_eq!(box_data.texture_index, 5);
            assert_eq!(box_data.light_owner, None);
            assert!(!box_data.direct_light);
        }
    }

    #[test]
    fn four_direct_sources_match_the_shader_index_table() {
        let boxes = scene_boxes();
        let expected = [
            (5, REDSTONE_SLOT),
            (9, COPPER_SLOT),
            (11, SOUL_SLOT),
            (13, TORCH_SLOT),
        ];
        for (index, slot) in expected {
            assert_eq!(boxes[index].light_owner, Some(slot));
            assert!(boxes[index].direct_light);
            assert!(boxes[index].light_level > 0.5);
        }
    }

    #[test]
    fn packed_box_flags_preserve_bank_owner_and_sheet_roles() {
        let boxes = scene_boxes();
        assert_eq!(
            boxes[0].flags() & REDSTONE_ON_ONLY_FLAG,
            REDSTONE_ON_ONLY_FLAG
        );
        assert_eq!(boxes[1].flags() & OWNER_MASK, 1);
        assert_ne!(boxes[1].flags() & REDSTONE_SHEET_FLAG, 0);
        assert_eq!(
            boxes[6].flags() & REDSTONE_OFF_ONLY_FLAG,
            REDSTONE_OFF_ONLY_FLAG
        );
        assert_eq!(boxes[6].flags() & OWNER_MASK, 0);
        assert_eq!(boxes[14].flags(), 0);
    }
}
