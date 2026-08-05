use std::{error::Error, time::Duration};

use rand::{Rng, distributions::StandardNormal};

use super::{FrameInfo, Module, RenderContext, RenderSize, web_surface_fragment_entry};

const BLOCK_LIST_RESOURCE: &str = "full_blocks.txt";
const BLOCK_TEXTURE_RESOURCE: &str = "full_blocks.png";
const ATLAS_DIMENSION: u32 = 512;
const ATLAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const TILE_DIMENSION: u32 = 16;
const BLOCK_HALF_SIZE: f32 = 128.0;
const TICK_SECONDS: f64 = 0.05;
const UNIFORM_BYTES: u64 = 32;

// Web module=1 adds a trail mark only once per 50 ms bucket. Keep that exact
// cadence for the persistent FBO, but optionally overlay its newest block at
// the bucket-normalized position: without it, high velocities leave visibly
// 20 Hz-spaced leading marks on a high-refresh Wayland wallpaper. Set false
// for strict Web behavior.
const SMOOTH_TRAIL_FRONT: bool = false;

/// Native configurations for Web modules 1 and 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvdBounceVariant {
    /// Web `module=1`: retain every 50 ms block position in an off-screen trail.
    Trail,
    /// Web `module=2`: clear and redraw one smoothly interpolated block each frame.
    Direct,
}

/// Native wgpu implementation of Web modules 1 and 2 (`dvd_bounce`).
///
/// Both variants share the original 50 ms, non-catch-up integer state machine.
/// `Trail` renders each changed position into a persistent RGBA target; `Direct`
/// instead renders the fractional position directly to the Wayland target.
pub struct DvdBounceModule {
    variant: DvdBounceVariant,
    resources: Option<VariantResources>,
    block_bind_group: Option<wgpu::BindGroup>,
    uniforms: Option<wgpu::Buffer>,
    slot_count: usize,
    state: BounceState,
}

struct DirectResources {
    pipeline: wgpu::RenderPipeline,
}

struct TrailResources {
    trail_pipeline: wgpu::RenderPipeline,
    copy_pipeline: wgpu::RenderPipeline,
    copy_bind_group_layout: wgpu::BindGroupLayout,
    surface: Option<TrailSurfaceResources>,
    smooth_front_pipeline: Option<wgpu::RenderPipeline>,
}

enum VariantResources {
    Direct(DirectResources),
    Trail(Box<TrailResources>),
}

struct TrailSurfaceResources {
    target: TrailTarget,
    copy_bind_group: wgpu::BindGroup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResourceNeeds {
    direct_pipeline: bool,
    trail_pipeline: bool,
    trail_texture: bool,
    trail_copy_resources: bool,
}

fn resource_needs(variant: DvdBounceVariant, smooth_trail_front: bool) -> ResourceNeeds {
    match variant {
        DvdBounceVariant::Direct => ResourceNeeds {
            direct_pipeline: true,
            trail_pipeline: false,
            trail_texture: false,
            trail_copy_resources: false,
        },
        DvdBounceVariant::Trail => ResourceNeeds {
            direct_pipeline: smooth_trail_front,
            trail_pipeline: true,
            trail_texture: true,
            trail_copy_resources: true,
        },
    }
}

impl DvdBounceModule {
    pub fn new(variant: DvdBounceVariant) -> Self {
        let mut random = rand::thread_rng();
        let velocity = initial_velocity(&mut random);

        Self {
            variant,
            resources: None,
            block_bind_group: None,
            uniforms: None,
            slot_count: 0,
            state: BounceState {
                position: [0, 0],
                velocity,
                last_tick: 0,
                changed_this_tick: false,
                slot_offset: [0.0, 0.0],
            },
        }
    }
}

impl Module for DvdBounceModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let slots = load_slots()?;
        let atlas = crate::resources::load_rgba_png(BLOCK_TEXTURE_RESOURCE)?;
        if atlas.dimensions() != (ATLAS_DIMENSION, ATLAS_DIMENSION) {
            return Err(format!(
                "{BLOCK_TEXTURE_RESOURCE} must be {ATLAS_DIMENSION}x{ATLAS_DIMENSION}, got {}x{}",
                atlas.width(),
                atlas.height()
            )
            .into());
        }

        let mut random = rand::thread_rng();
        self.state.slot_offset = slot_offset(random.gen_range(0, slots.len()));
        self.slot_count = slots.len();

        let atlas_texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dvd-bounce blocks atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_DIMENSION,
                height: ATLAS_DIMENSION,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // The WebGL upload uses RGB bytes without sRGB decode. The image
            // crate expands those bytes to opaque RGBA while preserving RGB.
            format: ATLAS_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        context.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            atlas.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_DIMENSION * 4),
                rows_per_image: Some(ATLAS_DIMENSION),
            },
            atlas_texture.size(),
        );
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let nearest_sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("dvd-bounce nearest sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let block_layout =
            context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("dvd-bounce block bind group layout"),
                    entries: &[
                        uniform_binding(0, wgpu::ShaderStages::VERTEX),
                        texture_binding(1),
                        sampler_binding(2),
                    ],
                });
        let block_pipeline_layout =
            context
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("dvd-bounce block pipeline layout"),
                    bind_group_layouts: &[Some(&block_layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("dvd-bounce shader"),
                source: wgpu::ShaderSource::Wgsl(DVD_BOUNCE_SHADER.into()),
            });
        let uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dvd-bounce uniforms"),
            size: UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let block_bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("dvd-bounce block bind group"),
                layout: &block_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&atlas_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&nearest_sampler),
                    },
                ],
            });

        let needs = resource_needs(self.variant, SMOOTH_TRAIL_FRONT);
        self.resources = Some(match self.variant {
            DvdBounceVariant::Direct => {
                debug_assert!(needs.direct_pipeline);
                VariantResources::Direct(DirectResources {
                    pipeline: create_block_pipeline(
                        context.device,
                        &shader,
                        &block_pipeline_layout,
                        context.surface_format,
                        "dvd-bounce direct pipeline",
                        web_surface_fragment_entry(
                            context.surface_format,
                            "fs_direct_srgb",
                            "fs_direct_unorm",
                        ),
                    ),
                })
            }
            DvdBounceVariant::Trail => {
                debug_assert!(
                    needs.trail_pipeline && needs.trail_texture && needs.trail_copy_resources
                );
                let copy_bind_group_layout =
                    context
                        .device
                        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                            label: Some("dvd-bounce copy bind group layout"),
                            entries: &[texture_binding(3), sampler_binding(4)],
                        });
                let copy_pipeline_layout =
                    context
                        .device
                        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                            label: Some("dvd-bounce copy pipeline layout"),
                            bind_group_layouts: &[Some(&copy_bind_group_layout)],
                            immediate_size: 0,
                        });
                let trail_pipeline = create_block_pipeline(
                    context.device,
                    &shader,
                    &block_pipeline_layout,
                    wgpu::TextureFormat::Rgba8Unorm,
                    "dvd-bounce trail pipeline",
                    "fs_trail",
                );
                let copy_pipeline =
                    context
                        .device
                        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                            label: Some("dvd-bounce copy pipeline"),
                            layout: Some(&copy_pipeline_layout),
                            vertex: wgpu::VertexState {
                                module: &shader,
                                entry_point: Some("vs_copy"),
                                buffers: &[],
                                compilation_options: Default::default(),
                            },
                            fragment: Some(wgpu::FragmentState {
                                module: &shader,
                                entry_point: Some(web_surface_fragment_entry(
                                    context.surface_format,
                                    "fs_copy_srgb",
                                    "fs_copy_unorm",
                                )),
                                compilation_options: Default::default(),
                                targets: &[Some(context.surface_format.into())],
                            }),
                            primitive: ccw_backface_primitive(),
                            depth_stencil: None,
                            multisample: wgpu::MultisampleState::default(),
                            multiview_mask: None,
                            cache: None,
                        });
                let smooth_front_pipeline = needs.direct_pipeline.then(|| {
                    create_block_pipeline(
                        context.device,
                        &shader,
                        &block_pipeline_layout,
                        context.surface_format,
                        "dvd-bounce smooth trail front pipeline",
                        web_surface_fragment_entry(
                            context.surface_format,
                            "fs_direct_srgb",
                            "fs_direct_unorm",
                        ),
                    )
                });
                VariantResources::Trail(Box::new(TrailResources {
                    trail_pipeline,
                    copy_pipeline,
                    copy_bind_group_layout,
                    surface: None,
                    smooth_front_pipeline,
                }))
            }
        });
        self.block_bind_group = Some(block_bind_group);
        self.uniforms = Some(uniforms);
        Ok(())
    }

    fn resize(&mut self, context: &RenderContext<'_>, size: RenderSize) {
        let resources = self
            .resources
            .as_mut()
            .expect("DvdBounceModule was not initialized");
        let VariantResources::Trail(resources) = resources else {
            return;
        };

        let target = TrailTarget::new(context.device, size);
        let sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("dvd-bounce trail nearest sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let copy_bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("dvd-bounce copy bind group"),
                layout: &resources.copy_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&target.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            });
        resources.surface = Some(TrailSurfaceResources {
            target,
            copy_bind_group,
        });
    }

    fn update(&mut self, frame: FrameInfo) {
        let tick = tick_for_elapsed(frame.elapsed);
        self.state.changed_this_tick = tick != self.state.last_tick;
        if !self.state.changed_this_tick {
            return;
        }
        self.state.last_tick = tick;
        self.state.position[0] += self.state.velocity[0];
        self.state.position[1] += self.state.velocity[1];

        let half_width = frame.size.width.max(1) as i32 / 2;
        let half_height = frame.size.height.max(1) as i32 / 2;
        let half_block = BLOCK_HALF_SIZE as i32;
        let bounced_x = self.state.position[0] - half_block < -half_width
            || self.state.position[0] + half_block > half_width;
        if bounced_x {
            self.state.velocity[0] = -self.state.velocity[0];
        }
        let bounced_y = self.state.position[1] - half_block < -half_height
            || self.state.position[1] + half_block > half_height;
        if bounced_y {
            self.state.velocity[1] = -self.state.velocity[1];
        }
        if bounced_x || bounced_y {
            // func[165] draws one unbiased random id in [0, 463), then
            // writes (id % 32, id / 32) to slot_offset before this frame's
            // block draw. A corner hit still performs exactly one reselection.
            let mut random = rand::thread_rng();
            self.state.slot_offset = slot_offset(random.gen_range(0, self.slot_count));
        }
    }

    fn wants_continuous_frames(&self) -> bool {
        true
    }

    fn render(
        &mut self,
        context: &RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        frame: FrameInfo,
    ) {
        let uniforms = self
            .uniforms
            .as_ref()
            .expect("DvdBounceModule was not initialized");
        let block_bind_group = self
            .block_bind_group
            .as_ref()
            .expect("DvdBounceModule was not initialized");
        let resources = self
            .resources
            .as_mut()
            .expect("DvdBounceModule was not initialized");

        match resources {
            VariantResources::Direct(resources) => {
                let fraction = interpolation_fraction(frame.elapsed);
                let center = [
                    self.state.position[0] as f32 + fraction * self.state.velocity[0] as f32,
                    self.state.position[1] as f32 + fraction * self.state.velocity[1] as f32,
                ];
                context.queue.write_buffer(
                    uniforms,
                    0,
                    &uniform_bytes(center, frame.size, self.state.slot_offset),
                );
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("dvd-bounce direct pass"),
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
                pass.set_pipeline(&resources.pipeline);
                pass.set_bind_group(0, block_bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
            VariantResources::Trail(resources) => {
                let surface = resources
                    .surface
                    .as_mut()
                    .expect("DvdBounceModule was not resized");
                if surface.target.clear_pending {
                    let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("dvd-bounce initial trail clear"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &surface.target.view,
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
                    drop(_pass);
                    surface.target.clear_pending = false;
                }
                if self.state.changed_this_tick {
                    context.queue.write_buffer(
                        uniforms,
                        0,
                        &uniform_bytes(
                            [self.state.position[0] as f32, self.state.position[1] as f32],
                            frame.size,
                            self.state.slot_offset,
                        ),
                    );
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("dvd-bounce persistent trail pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &surface.target.view,
                            depth_slice: None,
                            resolve_target: None,
                            // The Web FBO is cleared once at construction, not
                            // per tick. Loading preserves every previous block.
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(&resources.trail_pipeline);
                    pass.set_bind_group(0, block_bind_group, &[]);
                    pass.draw(0..6, 0..1);
                }

                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("dvd-bounce trail copy pass"),
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
                pass.set_pipeline(&resources.copy_pipeline);
                pass.set_bind_group(0, &surface.copy_bind_group, &[]);
                pass.draw(0..6, 0..1);
                drop(pass);

                if let Some(pipeline) = resources.smooth_front_pipeline.as_ref() {
                    let fraction = interpolation_fraction(frame.elapsed);
                    let center = [
                        self.state.position[0] as f32 + fraction * self.state.velocity[0] as f32,
                        self.state.position[1] as f32 + fraction * self.state.velocity[1] as f32,
                    ];
                    context.queue.write_buffer(
                        uniforms,
                        0,
                        &uniform_bytes(center, frame.size, self.state.slot_offset),
                    );
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("dvd-bounce smooth trail front pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: target,
                            depth_slice: None,
                            resolve_target: None,
                            // The preceding copy contains the 20 Hz Web trail.
                            // Only overlay the current leading block.
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(pipeline);
                    pass.set_bind_group(0, block_bind_group, &[]);
                    pass.draw(0..6, 0..1);
                }
            }
        }
    }
}

struct BounceState {
    position: [i32; 2],
    velocity: [i32; 2],
    last_tick: u64,
    changed_this_tick: bool,
    slot_offset: [f32; 2],
}

struct TrailTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    clear_pending: bool,
}

impl TrailTarget {
    fn new(device: &wgpu::Device, size: RenderSize) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dvd-bounce persistent trail texture"),
            size: wgpu::Extent3d {
                width: size.width.max(1),
                height: size.height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Keep WebGL's framebuffer values numeric; the copy pass performs
            // the final numeric-sRGB to Wayland-target conversion.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            _texture: texture,
            view,
            clear_pending: true,
        }
    }
}

fn create_block_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    format: wgpu::TextureFormat,
    label: &'static str,
    fragment_entry: &'static str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_block"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fragment_entry),
            compilation_options: Default::default(),
            targets: &[Some(format.into())],
        }),
        primitive: ccw_backface_primitive(),
        // The Web path disables DEPTH_TEST and never enables blending.
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

fn ccw_backface_primitive() -> wgpu::PrimitiveState {
    wgpu::PrimitiveState {
        topology: wgpu::PrimitiveTopology::TriangleList,
        strip_index_format: None,
        // WebGL enables CULL_FACE, retains its CCW default, and culls back faces.
        front_face: wgpu::FrontFace::Ccw,
        cull_mode: Some(wgpu::Face::Back),
        unclipped_depth: false,
        polygon_mode: wgpu::PolygonMode::Fill,
        conservative: false,
    }
}

fn uniform_binding(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn texture_binding(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            multisampled: false,
            view_dimension: wgpu::TextureViewDimension::D2,
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
        },
        count: None,
    }
}

fn sampler_binding(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn load_slots() -> Result<Vec<[f32; 2]>, Box<dyn Error>> {
    let layout = crate::resources::load_utf8(BLOCK_LIST_RESOURCE)?;
    let mut lines = layout.lines();
    let line_width = parse_header(lines.next(), "line width")?;
    let slot_count = parse_header(lines.next(), "slot count")?;
    if line_width != ATLAS_DIMENSION / TILE_DIMENSION {
        return Err(format!(
            "{BLOCK_LIST_RESOURCE} line width {line_width} does not describe the {ATLAS_DIMENSION}px atlas"
        )
        .into());
    }
    let mut slots = Vec::with_capacity(slot_count as usize);
    for (index, line) in lines.filter(|line| !line.trim().is_empty()).enumerate() {
        let mut columns = line.split_whitespace();
        let row = columns
            .next()
            .ok_or_else(|| format!("missing row in {BLOCK_LIST_RESOURCE} entry {index}"))?
            .parse::<u32>()?;
        let column = columns
            .next()
            .ok_or_else(|| format!("missing column in {BLOCK_LIST_RESOURCE} entry {index}"))?
            .parse::<u32>()?;
        if columns.next().is_none() || row >= line_width || column >= line_width {
            return Err(format!("invalid tile entry {index} in {BLOCK_LIST_RESOURCE}").into());
        }
        // The text file is row, column; GLSL's slot_offset is x, y.
        slots.push([column as f32, row as f32]);
    }
    if slots.len() != slot_count as usize {
        return Err(format!(
            "{BLOCK_LIST_RESOURCE} declares {slot_count} slots but contains {} entries",
            slots.len()
        )
        .into());
    }
    Ok(slots)
}

fn parse_header(value: Option<&str>, name: &str) -> Result<u32, Box<dyn Error>> {
    value
        .ok_or_else(|| format!("{BLOCK_LIST_RESOURCE} is missing {name}"))?
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("cannot parse {name} in {BLOCK_LIST_RESOURCE}: {error}").into())
}

fn tick_for_elapsed(elapsed: Duration) -> u64 {
    (elapsed.as_secs_f64() / TICK_SECONDS).floor() as u64
}

/// Position is advanced in whole 50 ms buckets. Rendering module=2 needs the
/// normalized progress through the current bucket, not elapsed seconds: using
/// the raw 0.0..0.05 remainder would visibly jump at 20 Hz.
fn interpolation_fraction(elapsed: Duration) -> f32 {
    let tick = tick_for_elapsed(elapsed) as f64;
    ((elapsed.as_secs_f64() - tick * TICK_SECONDS) / TICK_SECONDS) as f32
}

fn initial_velocity(rng: &mut impl Rng) -> [i32; 2] {
    loop {
        let candidate = [
            (rng.sample::<f64, _>(StandardNormal) * 7.0) as i32,
            (rng.sample::<f64, _>(StandardNormal) * 7.0) as i32,
        ];
        if valid_initial_velocity(candidate) {
            return candidate;
        }
    }
}

fn valid_initial_velocity([x, y]: [i32; 2]) -> bool {
    let x_squared = i64::from(x) * i64::from(x);
    let y_squared = i64::from(y) * i64::from(y);
    x != 0 && y != 0 && x_squared + y_squared > 24
}

fn slot_offset(id: usize) -> [f32; 2] {
    [
        (id % (ATLAS_DIMENSION / TILE_DIMENSION) as usize) as f32,
        (id / (ATLAS_DIMENSION / TILE_DIMENSION) as usize) as f32,
    ]
}

fn uniform_bytes(
    center: [f32; 2],
    size: RenderSize,
    slot_offset: [f32; 2],
) -> [u8; UNIFORM_BYTES as usize] {
    let values = [
        center[0],
        center[1],
        size.width.max(1) as f32,
        size.height.max(1) as f32,
        slot_offset[0],
        slot_offset[1],
        0.0,
        0.0,
    ];
    let mut bytes = [0; UNIFORM_BYTES as usize];
    for (index, value) in values.into_iter().enumerate() {
        bytes[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

const DVD_BOUNCE_SHADER: &str = r#"
struct DvdUniforms {
    center: vec2<f32>,
    viewport: vec2<f32>,
    slot_offset: vec2<f32>,
    _padding: vec2<f32>,
};

struct BlockVertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct CopyVertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: DvdUniforms;
@group(0) @binding(1) var blocks_texture: texture_2d<f32>;
@group(0) @binding(2) var blocks_sampler: sampler;

@vertex
fn vs_block(@builtin(vertex_index) index: u32) -> BlockVertexOutput {
    // Exact captured DVD VBO ordering: position.xy followed by vTex.xy.
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(-128.0, -128.0), vec2<f32>(128.0, -128.0), vec2<f32>(-128.0, 128.0),
        vec2<f32>(128.0, -128.0), vec2<f32>(128.0, 128.0), vec2<f32>(-128.0, 128.0),
    );
    let texcoords = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 0.0),
    );
    let pixel = uniforms.center + positions[index];
    var output: BlockVertexOutput;
    output.position = vec4<f32>(pixel * 2.0 / uniforms.viewport, 0.0, 1.0);
    output.uv = (uniforms.slot_offset + texcoords[index] * 0.999) / 32.0;
    return output;
}

fn srgb_to_linear(channel: f32) -> f32 {
    if (channel <= 0.04045) {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_direct_srgb(input: BlockVertexOutput) -> @location(0) vec4<f32> {
    let webgl_rgb = textureSample(blocks_texture, blocks_sampler, input.uv).rgb;
    return vec4<f32>(
        vec3<f32>(
            srgb_to_linear(webgl_rgb.r),
            srgb_to_linear(webgl_rgb.g),
            srgb_to_linear(webgl_rgb.b),
        ),
        1.0,
    );
}

@fragment
fn fs_direct_unorm(input: BlockVertexOutput) -> @location(0) vec4<f32> {
    let webgl_rgb = textureSample(blocks_texture, blocks_sampler, input.uv).rgb;
    return vec4<f32>(webgl_rgb, 1.0);
}

// The WebGL framebuffer stores the shader's unconverted numeric RGB. Preserve
// that representation in Rgba8Unorm; the surface copy performs conversion
// finally written to the Wayland surface.
@fragment
fn fs_trail(input: BlockVertexOutput) -> @location(0) vec4<f32> {
    return textureSample(blocks_texture, blocks_sampler, input.uv);
}

@group(0) @binding(3) var trail_texture: texture_2d<f32>;
@group(0) @binding(4) var trail_sampler: sampler;

@vertex
fn vs_copy(@builtin(vertex_index) index: u32) -> CopyVertexOutput {
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let texcoords = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 0.0),
    );
    var output: CopyVertexOutput;
    output.position = vec4<f32>(positions[index], 0.0, 1.0);
    output.uv = texcoords[index];
    return output;
}

@fragment
fn fs_copy_srgb(input: CopyVertexOutput) -> @location(0) vec4<f32> {
    let webgl_color = textureSample(trail_texture, trail_sampler, input.uv);
    return vec4<f32>(
        vec3<f32>(
            srgb_to_linear(webgl_color.r),
            srgb_to_linear(webgl_color.g),
            srgb_to_linear(webgl_color.b),
        ),
        1.0,
    );
}

@fragment
fn fs_copy_unorm(input: CopyVertexOutput) -> @location(0) vec4<f32> {
    let webgl_rgb = textureSample(trail_texture, trail_sampler, input.uv).rgb;
    return vec4<f32>(webgl_rgb, 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rand::SeedableRng;
    use rand_hc::Hc128Rng;

    use super::{
        DvdBounceVariant, RenderSize, ResourceNeeds, initial_velocity, interpolation_fraction,
        resource_needs, slot_offset, uniform_bytes, valid_initial_velocity,
    };

    #[test]
    fn variant_resource_needs_exclude_unused_gpu_allocations() {
        assert_eq!(
            resource_needs(DvdBounceVariant::Direct, false),
            ResourceNeeds {
                direct_pipeline: true,
                trail_pipeline: false,
                trail_texture: false,
                trail_copy_resources: false,
            }
        );
        assert_eq!(
            resource_needs(DvdBounceVariant::Direct, true),
            ResourceNeeds {
                direct_pipeline: true,
                trail_pipeline: false,
                trail_texture: false,
                trail_copy_resources: false,
            }
        );
        assert_eq!(
            resource_needs(DvdBounceVariant::Trail, false),
            ResourceNeeds {
                direct_pipeline: false,
                trail_pipeline: true,
                trail_texture: true,
                trail_copy_resources: true,
            }
        );
        assert_eq!(
            resource_needs(DvdBounceVariant::Trail, true),
            ResourceNeeds {
                direct_pipeline: true,
                trail_pipeline: true,
                trail_texture: true,
                trail_copy_resources: true,
            }
        );
    }

    #[test]
    fn slot_id_maps_to_the_webgl_column_and_row() {
        assert_eq!(slot_offset(0), [0.0, 0.0]);
        assert_eq!(slot_offset(31), [31.0, 0.0]);
        assert_eq!(slot_offset(32), [0.0, 1.0]);
        assert_eq!(slot_offset(462), [14.0, 14.0]);
    }

    #[test]
    fn standard_normal_initial_velocity_is_deterministic_for_a_fixed_seed() {
        let seed = [0x5a; 32];
        let first = initial_velocity(&mut Hc128Rng::from_seed(seed));
        let second = initial_velocity(&mut Hc128Rng::from_seed(seed));
        assert_eq!(first, second);
        assert_eq!(first, [-12, -15]);
        assert!(valid_initial_velocity(first));
    }

    #[test]
    fn initial_velocity_rejects_zero_components_and_short_vectors() {
        for rejected in [[0, 10], [10, 0], [1, 1], [3, 3], [4, 2]] {
            assert!(!valid_initial_velocity(rejected), "accepted {rejected:?}");
        }
        for accepted in [[1, 5], [-5, 1], [4, 3], [-4, -3]] {
            assert!(valid_initial_velocity(accepted), "rejected {accepted:?}");
        }
    }

    #[test]
    fn direct_interpolation_spans_the_entire_tick() {
        assert_eq!(interpolation_fraction(Duration::ZERO), 0.0);
        assert!((interpolation_fraction(Duration::from_millis(25)) - 0.5).abs() < 1e-6);
        assert!((interpolation_fraction(Duration::from_millis(49)) - 0.98).abs() < 1e-6);
        assert_eq!(interpolation_fraction(Duration::from_millis(50)), 0.0);
    }

    #[test]
    fn uniform_layout_matches_wgsl() {
        let bytes = uniform_bytes(
            [12.0, -4.0],
            RenderSize {
                width: 1280,
                height: 720,
            },
            [31.0, 14.0],
        );
        assert_eq!(&bytes[0..4], &12.0f32.to_ne_bytes());
        assert_eq!(&bytes[4..8], &(-4.0f32).to_ne_bytes());
        assert_eq!(&bytes[8..12], &1280.0f32.to_ne_bytes());
        assert_eq!(&bytes[12..16], &720.0f32.to_ne_bytes());
        assert_eq!(&bytes[16..20], &31.0f32.to_ne_bytes());
        assert_eq!(&bytes[20..24], &14.0f32.to_ne_bytes());
    }
}
