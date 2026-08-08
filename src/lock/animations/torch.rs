use std::error::Error;

use crate::{
    lock::state::{COPPER_BIT, REDSTONE_BIT, SOUL_BIT, TORCH_BIT},
    modules::{RenderContext, RenderSize},
};

const SHADER: &str = include_str!("torch.wgsl");
const ACCUMULATION_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const MAX_STATE_UPDATES: u32 = 512;
const UNIFORM_BYTES: u64 = 80;
const BOX_STRIDE: usize = 80;
const BOX_COUNT: usize = 16;

pub struct TorchAnimation {
    format: Option<wgpu::TextureFormat>,
    trace_pipeline: Option<wgpu::RenderPipeline>,
    present_pipeline: Option<wgpu::RenderPipeline>,
    trace_bind_group: Option<wgpu::BindGroup>,
    present_layout: Option<wgpu::BindGroupLayout>,
    uniforms: Option<wgpu::Buffer>,
    boxes: Option<wgpu::Buffer>,
    accumulation: Option<AccumulationTarget>,
    temporal: TemporalAccumulator,
    uploaded_mask: Option<u8>,
}

struct AccumulationTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
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
            uniforms: None,
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

    pub fn wants_continuous_frames(&self, state_id: u64) -> bool {
        self.temporal.needs_update(state_id)
    }

    pub fn draw(
        &mut self,
        context: &RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        size: RenderSize,
        mask: u8,
        state_id: u64,
    ) {
        self.ensure_initialized(context, size)
            .expect("torch animation initialized during surface configure");
        let state_changed = self.temporal.begin_state(state_id);
        if state_changed {
            self.uploaded_mask = None;
        }
        if self.uploaded_mask != Some(mask) {
            let boxes = self.boxes.as_ref().expect("torch box buffer initialized");
            context.queue.write_buffer(boxes, 0, &encoded_boxes(mask));
            self.uploaded_mask = Some(mask);
        }

        if let Some(update) = self.temporal.next_update() {
            let uniforms = self
                .uniforms
                .as_ref()
                .expect("torch uniform buffer initialized");
            context.queue.write_buffer(
                uniforms,
                0,
                &encoded_uniforms(size, update.sample_index, mask),
            );
            let accumulation = self
                .accumulation
                .as_ref()
                .expect("torch accumulation target initialized");
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("torch HDR temporal accumulation"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &accumulation.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: if state_changed {
                            wgpu::LoadOp::Clear(wgpu::Color::BLACK)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
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
            label: Some("torch tone-map and present"),
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
                depth_or_array_layers: 5,
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
        let uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("torch camera and temporal uniforms"),
            size: UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let boxes = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("torch scene box table"),
            size: (BOX_COUNT * BOX_STRIDE) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
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
                    entries: &[wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        },
                        count: None,
                    }],
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("torch ray tracing shader"),
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
        let trace_pipeline =
            context
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("torch HDR trace pipeline"),
                    layout: Some(&trace_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("fs"),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: ACCUMULATION_FORMAT,
                            blend: Some(wgpu::BlendState {
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
                            }),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
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
                    label: Some("torch tone-map pipeline"),
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
                        resource: uniforms.as_entire_binding(),
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
        self.uniforms = Some(uniforms);
        self.boxes = Some(boxes);
        self.accumulation = None;
        self.temporal.clear();
        self.uploaded_mask = None;
        Ok(())
    }

    fn rebuild_accumulation(&mut self, context: &RenderContext<'_>, size: RenderSize) {
        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("torch linear HDR temporal accumulation"),
            size: wgpu::Extent3d {
                width: size.width.max(1),
                height: size.height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ACCUMULATION_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let present_bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("torch accumulation present bind group"),
                layout: self
                    .present_layout
                    .as_ref()
                    .expect("torch present layout initialized"),
                entries: &[wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&view),
                }],
            });
        self.accumulation = Some(AccumulationTarget {
            _texture: texture,
            view,
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
    state_id: Option<u64>,
    state_updates: u32,
}

impl TemporalAccumulator {
    fn resize(&mut self, size: RenderSize) -> bool {
        if self.size == Some(size) {
            return false;
        }
        self.size = Some(size);
        self.state_id = None;
        self.state_updates = 0;
        true
    }

    fn clear(&mut self) {
        self.size = None;
        self.state_id = None;
        self.state_updates = 0;
    }

    fn begin_state(&mut self, state_id: u64) -> bool {
        if self.state_id == Some(state_id) {
            return false;
        }
        self.state_id = Some(state_id);
        self.state_updates = 0;
        true
    }

    fn needs_update(&self, state_id: u64) -> bool {
        self.size.is_some()
            && (self.state_id != Some(state_id) || self.state_updates < MAX_STATE_UPDATES)
    }

    fn next_update(&mut self) -> Option<TemporalUpdate> {
        if self.state_updates >= MAX_STATE_UPDATES {
            return None;
        }
        let update = TemporalUpdate {
            sample_index: self.state_updates,
            new_sample_weight: 1.0 / (self.state_updates + 1) as f32,
        };
        self.state_updates += 1;
        Some(update)
    }
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
    torch_bit: u8,
}

fn encoded_boxes(mask: u8) -> Vec<u8> {
    let mut bytes = vec![0; BOX_COUNT * BOX_STRIDE];
    for (index, box_data) in scene_boxes().iter().enumerate() {
        let base = index * BOX_STRIDE;
        write_vec3(&mut bytes, base, box_data.min);
        write_vec3(&mut bytes, base + 16, box_data.max);
        write_f32(&mut bytes, base + 32, box_data.light_level);
        write_f32(&mut bytes, base + 36, box_data.smoothness);
        write_u32(&mut bytes, base + 40, box_data.texture_index);
        let enabled = u32::from(box_data.torch_bit == 0 || mask & box_data.torch_bit != 0);
        write_u32(&mut bytes, base + 44, enabled);
        write_vec3(&mut bytes, base + 48, box_data.texture_offset_positive);
        write_vec3(&mut bytes, base + 64, box_data.texture_offset_negative);
    }
    bytes
}

fn encoded_uniforms(size: RenderSize, sample_index: u32, mask: u8) -> [u8; UNIFORM_BYTES as usize] {
    let mut bytes = [0; UNIFORM_BYTES as usize];
    write_vec3(&mut bytes, 0, [1.0, 0.0, 0.0]);
    write_vec3(&mut bytes, 16, [0.0, 0.0, 1.0]);
    write_f32(&mut bytes, 32, 70.0_f32.to_radians());
    write_vec3(&mut bytes, 48, [0.0, -3.0, 1.0]);
    write_f32(&mut bytes, 64, size.width as f32);
    write_f32(&mut bytes, 68, size.height as f32);
    write_u32(&mut bytes, 72, sample_index);
    write_u32(&mut bytes, 76, u32::from(mask));
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
    let torch_box = |min, max, light_level, texture_index, torch_bit| BoxData {
        min,
        max,
        light_level,
        smoothness: 0.0,
        texture_index,
        texture_offset_positive: TEX_POS,
        texture_offset_negative: TEX_NEG,
        torch_bit,
    };
    let ground = |min, max| BoxData {
        min,
        max,
        light_level: 0.0,
        smoothness: 0.2,
        texture_index: 4,
        texture_offset_positive: NO_OFFSET,
        texture_offset_negative: NO_OFFSET,
        torch_bit: 0,
    };
    [
        torch_box(
            [-0.5625, 0.4375, 0.0],
            [-0.4375, 0.5625, 0.4375],
            0.0,
            0,
            REDSTONE_BIT,
        ),
        torch_box(
            [-1.0, 0.4375, 0.0],
            [0.0, 0.4375, 1.0],
            7.0,
            0,
            REDSTONE_BIT,
        ),
        torch_box(
            [-1.0, 0.5625, 0.0],
            [0.0, 0.5625, 1.0],
            7.0,
            0,
            REDSTONE_BIT,
        ),
        torch_box(
            [-0.5625, 0.0, 0.0],
            [-0.5625, 1.0, 1.0],
            7.0,
            0,
            REDSTONE_BIT,
        ),
        torch_box(
            [-0.4375, 0.0, 0.0],
            [-0.4375, 1.0, 1.0],
            7.0,
            0,
            REDSTONE_BIT,
        ),
        torch_box(
            [-0.5625, 0.4375, 0.5],
            [-0.4375, 0.5625, 0.625],
            7.0,
            0,
            REDSTONE_BIT,
        ),
        torch_box(
            [0.4375, 0.4375, 0.0],
            [0.5625, 0.5625, 0.5],
            0.0,
            1,
            COPPER_BIT,
        ),
        torch_box(
            [0.4375, 0.4375, 0.5],
            [0.5625, 0.5625, 0.625],
            14.0,
            1,
            COPPER_BIT,
        ),
        torch_box(
            [-0.5625, -0.5625, 0.0],
            [-0.4375, -0.4375, 0.5],
            0.0,
            2,
            SOUL_BIT,
        ),
        torch_box(
            [-0.5625, -0.5625, 0.5],
            [-0.4375, -0.4375, 0.625],
            10.0,
            2,
            SOUL_BIT,
        ),
        torch_box(
            [0.4375, -0.5625, 0.0],
            [0.5625, -0.4375, 0.5],
            0.0,
            3,
            TORCH_BIT,
        ),
        torch_box(
            [0.4375, -0.5625, 0.5],
            [0.5625, -0.4375, 0.625],
            14.0,
            3,
            TORCH_BIT,
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
    fn state_switch_discards_history_and_restarts_running_mean() {
        let mut temporal = TemporalAccumulator::default();
        assert!(temporal.resize(RenderSize {
            width: 10,
            height: 8
        }));
        assert!(temporal.begin_state(1));
        assert_eq!(
            temporal.next_update().unwrap(),
            TemporalUpdate {
                sample_index: 0,
                new_sample_weight: 1.0
            }
        );
        assert_eq!(
            temporal.next_update().unwrap(),
            TemporalUpdate {
                sample_index: 1,
                new_sample_weight: 0.5
            }
        );

        assert!(temporal.begin_state(2));
        assert_eq!(
            temporal.next_update().unwrap(),
            TemporalUpdate {
                sample_index: 0,
                new_sample_weight: 1.0
            }
        );
        assert_eq!(
            temporal.next_update().unwrap(),
            TemporalUpdate {
                sample_index: 1,
                new_sample_weight: 0.5
            }
        );
    }

    #[test]
    fn resize_resets_only_the_changed_outputs_cache() {
        let mut temporal = TemporalAccumulator::default();
        let first = RenderSize {
            width: 10,
            height: 8,
        };
        assert!(temporal.resize(first));
        temporal.begin_state(7);
        temporal.next_update();
        assert!(!temporal.resize(first));
        assert_eq!(temporal.state_updates, 1);

        assert!(temporal.resize(RenderSize {
            width: 11,
            height: 8
        }));
        assert_eq!(temporal.state_updates, 0);
        assert_eq!(temporal.state_id, None);
    }

    #[test]
    fn continuous_frames_stop_after_512_new_state_updates() {
        let mut temporal = TemporalAccumulator::default();
        temporal.resize(RenderSize {
            width: 1,
            height: 1,
        });
        temporal.begin_state(9);
        for expected in 0..MAX_STATE_UPDATES {
            assert!(temporal.needs_update(9));
            assert_eq!(temporal.next_update().unwrap().sample_index, expected);
        }
        assert!(!temporal.needs_update(9));
        assert_eq!(temporal.next_update(), None);
        assert!(temporal.needs_update(10));
    }

    #[test]
    fn mask_disables_every_box_for_each_extinguished_torch_but_never_ground() {
        let bytes = encoded_boxes(COPPER_BIT);
        for (index, box_data) in scene_boxes().iter().enumerate() {
            let enabled = u32::from_ne_bytes(
                bytes[index * BOX_STRIDE + 44..index * BOX_STRIDE + 48]
                    .try_into()
                    .unwrap(),
            );
            let expected = u32::from(box_data.torch_bit == 0 || box_data.torch_bit == COPPER_BIT);
            assert_eq!(enabled, expected);
        }
    }
}
