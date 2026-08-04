use std::error::Error;

use rand::Rng;

use super::{FrameInfo, Module, RenderContext, RenderSize};

const FOOTPRINT_RESOURCE: &str = "footprint.png";
const FOOTPRINT_DIMENSION: u32 = 8;
const TICK_SECONDS: f64 = 0.05;
const WALKER_COUNT: usize = 4;
const MAX_FOOTPRINTS: usize = 64;
const FOOTPRINT_BYTES: u64 = 32;
const GLOBAL_BYTES: u64 = 16;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Native wgpu implementation of Web module=7 (`footprint`).
///
/// The Web module advances four walkers no more than once for each 50 ms time
/// bucket.  Their position is intentionally not caught up after a delayed
/// frame callback: a skipped bucket means a skipped movement and footprint.
#[derive(Default)]
pub struct FootprintModule {
    pipeline: Option<wgpu::RenderPipeline>,
    bind_group: Option<wgpu::BindGroup>,
    globals: Option<wgpu::Buffer>,
    footprints: Option<wgpu::Buffer>,
    depth: Option<DepthTarget>,
    state: FootprintState,
}

impl Module for FootprintModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let image = crate::resources::load_rgba_png(FOOTPRINT_RESOURCE)?;
        if image.dimensions() != (FOOTPRINT_DIMENSION, FOOTPRINT_DIMENSION) {
            return Err(format!(
                "{FOOTPRINT_RESOURCE} must be {FOOTPRINT_DIMENSION}x{FOOTPRINT_DIMENSION}, got {}x{}",
                image.width(),
                image.height()
            )
            .into());
        }

        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("footprint texture"),
            size: wgpu::Extent3d {
                width: FOOTPRINT_DIMENSION,
                height: FOOTPRINT_DIMENSION,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // WebGL receives the PNG's decoded RGB values directly.  Keep the
            // sampled numeric values unmodified rather than applying sRGB
            // decoding at the texture boundary.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        context.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            image.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(FOOTPRINT_DIMENSION * 4),
                rows_per_image: Some(FOOTPRINT_DIMENSION),
            },
            texture.size(),
        );
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("footprint nearest sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let layout = context
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("footprint bind group layout"),
                entries: &[
                    uniform_layout_entry(0, wgpu::ShaderStages::VERTEX),
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let pipeline_layout =
            context
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("footprint pipeline layout"),
                    bind_group_layouts: &[Some(&layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("footprint shader"),
                source: wgpu::ShaderSource::Wgsl(FOOTPRINT_SHADER.into()),
            });
        let globals = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("footprint globals"),
            size: GLOBAL_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let footprints = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("footprint instances"),
            size: MAX_FOOTPRINTS as u64 * FOOTPRINT_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("footprint bind group"),
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: globals.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: footprints.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            });

        self.pipeline = Some(context.device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("footprint pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: context.surface_format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    // The captured two-triangle footprint VBO is CCW and the
                    // VP transform has no handedness reversal.
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: Some(wgpu::Face::Back),
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                // WebGL enables DEPTH_TEST and retains the default LESS
                // comparison/depth writes.  Every footprint has equal depth,
                // so earlier drawn overlapping texels occlude later ones.
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            },
        ));
        self.globals = Some(globals);
        self.footprints = Some(footprints);
        self.bind_group = Some(bind_group);
        Ok(())
    }

    fn resize(&mut self, context: &RenderContext<'_>, size: RenderSize) {
        self.state.set_viewport(size);
        self.depth = Some(DepthTarget::new(context.device, size));
    }

    fn update(&mut self, frame: FrameInfo) {
        self.state.advance(frame.elapsed.as_secs_f64());
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
        let pipeline = self
            .pipeline
            .as_ref()
            .expect("FootprintModule was not initialized");
        let bind_group = self
            .bind_group
            .as_ref()
            .expect("FootprintModule was not initialized");
        let globals = self
            .globals
            .as_ref()
            .expect("FootprintModule was not initialized");
        let footprints = self
            .footprints
            .as_ref()
            .expect("FootprintModule was not initialized");
        let depth = self
            .depth
            .as_ref()
            .expect("FootprintModule was not resized");

        // The CPU state gate uses floor(elapsed / 0.05), but the WebGL
        // `time` uniform retains the fractional tick for a smooth alpha fade.
        let shader_time = frame.elapsed.as_secs_f32() / TICK_SECONDS as f32;
        context.queue.write_buffer(
            globals,
            0,
            &global_bytes(
                shader_time,
                frame.size.width.max(1) as f32,
                frame.size.height.max(1) as f32,
            ),
        );
        context
            .queue
            .write_buffer(footprints, 0, &footprint_bytes(&self.state.footprints));

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("footprint module"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth.view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..6, 0..self.state.footprints.len() as u32);
    }
}

#[derive(Clone, Copy)]
struct Footprint {
    center: [f32; 2],
    rotation_degrees: f32,
    birth_tick: u64,
    color: [f32; 3],
}

struct Walker {
    position: [f32; 2],
    color: [f32; 3],
    direction: [f32; 2],
    rotation_degrees: f32,
    last_footprint_tick: u64,
    left_foot: bool,
    phase: WalkerPhase,
}

enum WalkerPhase {
    Paused {
        remaining: u64,
    },
    Moving {
        origin: [f32; 2],
        direction: [f32; 2],
        progress: u64,
        duration: u64,
        rotation_degrees: f32,
    },
}

struct FootprintState {
    last_tick: Option<u64>,
    viewport: RenderSize,
    walkers: [Walker; WALKER_COUNT],
    footprints: Vec<Footprint>,
}

impl Default for FootprintState {
    fn default() -> Self {
        Self {
            // The Web state is zero-initialized, so its first RAF at elapsed
            // time zero observes the same stored/current tick and does no
            // walker update.
            last_tick: Some(0),
            viewport: RenderSize {
                width: 1,
                height: 1,
            },
            walkers: [
                Walker::new([20.0, 20.0], [0.0, 0.0, 1.0]),
                Walker::new([-10.0, -10.0], [1.0, 1.0, 0.0]),
                Walker::new([10.0, 10.0], [0.0, 1.0, 0.0]),
                Walker::new([0.0, 0.0], [1.0, 0.0, 0.0]),
            ],
            footprints: Vec::with_capacity(MAX_FOOTPRINTS),
        }
    }
}

impl Walker {
    fn new(position: [f32; 2], color: [f32; 3]) -> Self {
        Self {
            position,
            color,
            direction: [0.0, 1.0],
            rotation_degrees: 0.0,
            // The Web state starts at zero, so a first tick below seven does
            // not leave a footprint.
            last_footprint_tick: 0,
            left_foot: false,
            phase: WalkerPhase::Paused { remaining: 0 },
        }
    }
}

impl FootprintState {
    fn set_viewport(&mut self, viewport: RenderSize) {
        self.viewport = viewport;
    }

    fn advance(&mut self, elapsed_seconds: f64) {
        let tick = (elapsed_seconds / TICK_SECONDS).floor() as u64;
        if self.last_tick == Some(tick) {
            return;
        }
        self.last_tick = Some(tick);

        let mut rng = rand::thread_rng();
        let bounds = Bounds::for_viewport(self.viewport);
        for walker in &mut self.walkers {
            advance_walker(walker, bounds, &mut rng);
            if tick.saturating_sub(walker.last_footprint_tick) >= 7 {
                walker.last_footprint_tick = tick;
                walker.left_foot = !walker.left_foot;
                self.footprints.push(make_footprint(walker, tick));
            }
        }
        self.footprints
            .retain(|footprint| tick.saturating_sub(footprint.birth_tick) <= 99);
        debug_assert!(self.footprints.len() <= MAX_FOOTPRINTS);
    }
}

#[derive(Clone, Copy)]
struct Bounds {
    min_x: f32,
    max_x: f32,
    min_y: f32,
    max_y: f32,
}

impl Bounds {
    fn for_viewport(size: RenderSize) -> Self {
        let half_width = size.width.max(1) as f32 * 0.04;
        let half_height = size.height.max(1) as f32 * 0.04;
        Self {
            min_x: -half_width,
            max_x: half_width,
            min_y: -half_height,
            max_y: half_height,
        }
    }

    fn contains_strictly(self, point: [f32; 2]) -> bool {
        self.min_x < point[0]
            && point[0] < self.max_x
            && self.min_y < point[1]
            && point[1] < self.max_y
    }
}

fn advance_walker(walker: &mut Walker, bounds: Bounds, rng: &mut impl Rng) {
    match walker.phase {
        WalkerPhase::Moving {
            origin,
            direction,
            progress,
            duration,
            ..
        } if progress + 1 < duration => {
            let progress = progress + 1;
            walker.position = [
                origin[0] + direction[0] * progress as f32 / 7.0,
                origin[1] + direction[1] * progress as f32 / 7.0,
            ];
            if let WalkerPhase::Moving {
                progress: stored, ..
            } = &mut walker.phase
            {
                *stored = progress;
            }
        }
        WalkerPhase::Moving { .. } => {
            walker.phase = WalkerPhase::Paused {
                remaining: rng.gen_range(20_u64, 40_u64),
            };
        }
        WalkerPhase::Paused { remaining } if remaining > 1 => {
            walker.phase = WalkerPhase::Paused {
                remaining: remaining - 1,
            };
        }
        WalkerPhase::Paused { .. } => {
            let phase = choose_run(walker.position, bounds, rng);
            if let WalkerPhase::Moving {
                direction,
                rotation_degrees,
                ..
            } = phase
            {
                walker.direction = direction;
                walker.rotation_degrees = rotation_degrees;
            }
            walker.phase = phase;
        }
    }
}

fn choose_run(position: [f32; 2], bounds: Bounds, rng: &mut impl Rng) -> WalkerPhase {
    loop {
        let rotation_degrees = rng.gen_range(0.0_f32, 360.0_f32);
        let radians = rotation_degrees.to_radians();
        let direction = [radians.cos(), radians.sin()];
        let duration = rng.gen_range(100_u64, 300_u64);
        let endpoint = [
            position[0] + direction[0] * duration as f32 / 7.0,
            position[1] + direction[1] * duration as f32 / 7.0,
        ];
        if bounds.contains_strictly(endpoint) {
            return WalkerPhase::Moving {
                origin: position,
                direction,
                progress: 0,
                duration,
                rotation_degrees,
            };
        }
    }
}

fn make_footprint(walker: &Walker, birth_tick: u64) -> Footprint {
    // The Web byte flag changes from 0 to 1 before selecting its side; that
    // first selected side is -0.75, hence the sign order here.
    let side = if walker.left_foot { -0.75 } else { 0.75 };
    Footprint {
        center: [
            walker.position[0] - side * walker.direction[1],
            walker.position[1] + side * walker.direction[0],
        ],
        rotation_degrees: walker.rotation_degrees,
        birth_tick,
        color: walker.color,
    }
}

struct DepthTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl DepthTarget {
    fn new(device: &wgpu::Device, size: RenderSize) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("footprint depth texture"),
            size: wgpu::Extent3d {
                width: size.width.max(1),
                height: size.height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            _texture: texture,
            view,
        }
    }
}

fn uniform_layout_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
) -> wgpu::BindGroupLayoutEntry {
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

fn global_bytes(shader_time: f32, width: f32, height: f32) -> [u8; GLOBAL_BYTES as usize] {
    let mut bytes = [0; GLOBAL_BYTES as usize];
    // WebGL uses VP=diag(2/width, 2/height, 1) and gl_Position.w=0.1;
    // emitting NDC directly therefore needs 20/width and 20/height.
    for (offset, value) in [
        (0, shader_time),
        (4, 20.0 / width),
        (8, 20.0 / height),
        (12, 0.0),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn footprint_bytes(footprints: &[Footprint]) -> Vec<u8> {
    let mut bytes = vec![0; MAX_FOOTPRINTS * FOOTPRINT_BYTES as usize];
    for (index, footprint) in footprints.iter().enumerate() {
        let offset = index * FOOTPRINT_BYTES as usize;
        write_f32(&mut bytes, offset, footprint.center[0]);
        write_f32(&mut bytes, offset + 4, footprint.center[1]);
        write_f32(
            &mut bytes,
            offset + 8,
            -footprint.rotation_degrees.to_radians(),
        );
        write_f32(&mut bytes, offset + 12, footprint.birth_tick as f32);
        write_f32(&mut bytes, offset + 16, footprint.color[0]);
        write_f32(&mut bytes, offset + 20, footprint.color[1]);
        write_f32(&mut bytes, offset + 24, footprint.color[2]);
    }
    bytes
}

fn write_f32(bytes: &mut [u8], offset: usize, value: f32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

const FOOTPRINT_SHADER: &str = r#"
struct Globals {
    tick: f32,
    clip_scale_x: f32,
    clip_scale_y: f32,
    _padding: f32,
};

struct Footprint {
    center: vec2<f32>,
    rotation: f32,
    birth_tick: f32,
    color: vec3<f32>,
    _padding: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex: vec2<f32>,
    @location(1) color: vec3<f32>,
    @location(2) alpha: f32,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<storage, read> footprints: array<Footprint>;
@group(0) @binding(2) var footprint_texture: texture_2d<f32>;
@group(0) @binding(3) var footprint_sampler: sampler;

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(-0.5, -0.5), vec2<f32>(0.5, -0.5), vec2<f32>(-0.5, 0.5),
        vec2<f32>(0.5, -0.5), vec2<f32>(0.5, 0.5), vec2<f32>(-0.5, 0.5),
    );
    let texcoords = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let footprint = footprints[instance_index];
    let s = sin(footprint.rotation);
    let c = cos(footprint.rotation);
    let position = positions[vertex_index];
    let rotated = vec2<f32>(
        c * position.x - s * position.y,
        s * position.x + c * position.y,
    );

    var output: VertexOutput;
    let clip_scale = vec2<f32>(globals.clip_scale_x, globals.clip_scale_y);
    output.position = vec4<f32>((rotated + footprint.center) * clip_scale, 0.5, 1.0);
    output.tex = texcoords[vertex_index];
    output.color = footprint.color;
    output.alpha = mix(0.75, 0.0, (globals.tick - footprint.birth_tick) / 100.0);
    return output;
}

// The WebGL canvas presents shader numeric RGB as sRGB; this Wayland target
// may be sRGB, so provide linear RGB for its encoding step.
fn srgb_to_linear(channel: f32) -> f32 {
    if (channel <= 0.04045) {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    if (input.alpha == 0.0) {
        discard;
    }
    let sampled = textureSample(footprint_texture, footprint_sampler, input.tex);
    let webgl_rgb = sampled.rgb * input.color;
    return vec4<f32>(
        vec3<f32>(
            srgb_to_linear(webgl_rgb.r),
            srgb_to_linear(webgl_rgb.g),
            srgb_to_linear(webgl_rgb.b),
        ),
        sampled.a * input.alpha,
    );
}
"#;
