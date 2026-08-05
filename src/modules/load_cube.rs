use std::error::Error;

use rand::{RngCore, SeedableRng};
use rand_hc::Hc128Rng;

use super::{FrameInfo, Module, RenderContext, RenderSize, web_surface_fragment_entry};

const ATLAS_RESOURCE: &str = "full_blocks.png";
const ATLAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const BLOCK_LIST_RESOURCE: &str = "full_blocks.txt";
const VISIBLE_FACE_COUNT: usize = 5;
const STORED_OFFSET_COUNT: usize = 6;
const TURN_DURATION_SECONDS: f32 = 0.5;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Native implementation of Web module=0 (`load_cube`).
///
/// The Web module turns a five-face cube by one eased quarter-turn every 500
/// ms. At each boundary the outgoing directional face becomes the front face,
/// its old slot receives one newly selected atlas block, and a new direction
/// is selected for the following turn.
pub struct LoadCubeModule {
    pipeline: Option<wgpu::RenderPipeline>,
    bind_group: Option<wgpu::BindGroup>,
    uniforms: Option<wgpu::Buffer>,
    depth: Option<DepthTarget>,
    state: CubeState,
}

impl Default for LoadCubeModule {
    fn default() -> Self {
        Self {
            pipeline: None,
            bind_group: None,
            uniforms: None,
            depth: None,
            // Replaced with the parsed atlas specification in initialize().
            state: CubeState::new(32, 463, [0; 32]),
        }
    }
}

impl Module for LoadCubeModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let atlas = crate::resources::load_rgba_png(ATLAS_RESOURCE)?;
        let atlas_spec = read_atlas_spec()?;
        let expected_dimension = atlas_spec
            .line_width
            .checked_mul(16)
            .ok_or("atlas width overflow")?;
        if atlas.dimensions() != (expected_dimension, expected_dimension) {
            return Err(format!(
                "{ATLAS_RESOURCE} must be {expected_dimension}x{expected_dimension}, got {}x{}",
                atlas.width(),
                atlas.height()
            )
            .into());
        }

        self.state = CubeState::new(atlas_spec.line_width, atlas_spec.slot_count, random_seed());

        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("load-cube block atlas"),
            size: wgpu::Extent3d {
                width: atlas.width(),
                height: atlas.height(),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // WebGL samples the decoded atlas bytes as numeric values, with no
            // sRGB decode at the texture boundary.
            format: ATLAS_FORMAT,
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
            atlas.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atlas.width() * 4),
                rows_per_image: Some(atlas.height()),
            },
            texture.size(),
        );
        let atlas_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas_sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("load-cube block atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let uniform_layout =
            context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("load-cube bind group layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                multisampled: false,
                                view_dimension: wgpu::TextureViewDimension::D2,
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
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
                    label: Some("load-cube pipeline layout"),
                    bind_group_layouts: &[Some(&uniform_layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("load-cube shader"),
                source: wgpu::ShaderSource::Wgsl(LOAD_CUBE_SHADER.into()),
            });
        let uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("load-cube uniforms"),
            size: UNIFORM_BYTES as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.bind_group = Some(
            context
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("load-cube bind group"),
                    layout: &uniform_layout,
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
                            resource: wgpu::BindingResource::Sampler(&atlas_sampler),
                        },
                    ],
                }),
        );
        self.uniforms = Some(uniforms);
        self.pipeline = Some(context.device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("load-cube pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(web_surface_fragment_entry(
                        context.surface_format,
                        "fs_srgb",
                        "fs_unorm",
                    )),
                    compilation_options: Default::default(),
                    targets: &[Some(context.surface_format.into())],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    // The captured WebGL MVP mirrors X. WebGL's lower-left
                    // framebuffer coordinates then make its `frontFace(CCW)`
                    // cull the opposite winding from WebGPU's upper-left
                    // render-target coordinates. Use CW to preserve the
                    // original visible-face / turn direction.
                    front_face: wgpu::FrontFace::Cw,
                    cull_mode: Some(wgpu::Face::Back),
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
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
        Ok(())
    }

    fn resize(&mut self, context: &RenderContext<'_>, size: RenderSize) {
        self.depth = Some(DepthTarget::new(context.device, size));
    }

    fn update(&mut self, frame: FrameInfo) {
        self.state.advance(frame.elapsed.as_secs_f32());
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
            .expect("LoadCubeModule was not initialized");
        let bind_group = self
            .bind_group
            .as_ref()
            .expect("LoadCubeModule was not initialized");
        let uniforms = self
            .uniforms
            .as_ref()
            .expect("LoadCubeModule was not initialized");
        let depth = self.depth.as_ref().expect("LoadCubeModule was not resized");
        let matrix = self.state.mvp(frame.elapsed.as_secs_f32(), frame.size);
        context.queue.write_buffer(
            uniforms,
            0,
            &uniform_bytes(
                matrix,
                self.state.visible_offsets(),
                self.state.line_width as f32,
            ),
        );

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("load-cube module"),
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
        pass.draw(0..30, 0..1);
    }
}

struct DepthTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl DepthTarget {
    fn new(device: &wgpu::Device, size: RenderSize) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("load-cube depth texture"),
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

#[derive(Clone, Copy)]
struct AtlasSpec {
    line_width: u32,
    slot_count: u32,
}

fn read_atlas_spec() -> Result<AtlasSpec, Box<dyn Error>> {
    let text = crate::resources::load_utf8(BLOCK_LIST_RESOURCE)?;
    let mut lines = text.lines();
    let line_width = lines
        .next()
        .ok_or("full_blocks.txt is missing its line width")?
        .parse::<u32>()?;
    let slot_count = lines
        .next()
        .ok_or("full_blocks.txt is missing its slot count")?
        .parse::<u32>()?;
    if line_width == 0 || slot_count == 0 {
        return Err("full_blocks.txt has a zero line width or slot count".into());
    }
    if lines.filter(|line| !line.trim().is_empty()).count() != slot_count as usize {
        return Err("full_blocks.txt slot count does not match its records".into());
    }
    Ok(AtlasSpec {
        line_width,
        slot_count,
    })
}

/// State and transition ordering recovered from the original Web module.
struct CubeState {
    line_width: u32,
    slot_count: u32,
    offsets: [[f32; 2]; STORED_OFFSET_COUNT],
    direction: u8,
    last_run: u64,
    random: Hc128Rng,
}

impl CubeState {
    fn new(line_width: u32, slot_count: u32, seed: [u8; 32]) -> Self {
        let mut random = Hc128Rng::from_seed(seed);
        let mut offsets = [[0.0; 2]; STORED_OFFSET_COUNT];
        for offset in &mut offsets {
            *offset = random_offset(&mut random, line_width, slot_count);
        }
        Self {
            line_width,
            slot_count,
            offsets,
            direction: random_direction(&mut random),
            last_run: 0,
            random,
        }
    }

    fn advance(&mut self, elapsed_seconds: f32) {
        let run = (elapsed_seconds.max(0.0) / TURN_DURATION_SECONDS).floor() as u64;
        if run != self.last_run {
            let side = usize::from(self.direction);
            self.offsets[0] = self.offsets[side];
            self.offsets[side] = random_offset(&mut self.random, self.line_width, self.slot_count);
            self.direction = random_direction(&mut self.random);
            self.last_run = run;
        }
    }

    fn visible_offsets(&self) -> &[[f32; 2]; VISIBLE_FACE_COUNT] {
        self.offsets[..VISIBLE_FACE_COUNT]
            .try_into()
            .expect("visible offsets have a fixed length")
    }

    fn mvp(&self, elapsed_seconds: f32, size: RenderSize) -> [f32; 16] {
        let u = (elapsed_seconds.max(0.0) / TURN_DURATION_SECONDS).fract();
        let eased = smootherstep(u);
        let angle = std::f32::consts::FRAC_PI_2 * eased;
        let rotation = match self.direction {
            1 => rotation_x(-angle),
            2 => rotation_x(angle),
            3 => rotation_y(-angle),
            4 => rotation_y(angle),
            _ => unreachable!("CubeState direction is always 1..=4"),
        };
        multiply(base_projection(size), rotation)
    }
}

/// The original Web module's `rand_hc` core consumes 32 bytes of entropy at
/// startup. Use the same HC-128 core and seed width; native entropy means a
/// launch is not expected to reproduce a particular Web launch's sequence.
fn random_seed() -> [u8; 32] {
    let mut seed = [0; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    seed
}

fn random_offset(random: &mut Hc128Rng, line_width: u32, slot_count: u32) -> [f32; 2] {
    let zone = slot_count << slot_count.leading_zeros();
    let slot = loop {
        let product = u64::from(slot_count) * u64::from(random.next_u32());
        if (product as u32) < zone {
            break (product >> 32) as u32;
        }
    };
    let row = slot / line_width;
    [(slot - row * line_width) as f32, row as f32]
}

fn random_direction(random: &mut Hc128Rng) -> u8 {
    loop {
        let value = random.next_u32();
        if value & (1 << 29) == 0 {
            return (value >> 30) as u8 + 1;
        }
    }
}

fn smootherstep(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    value * value * value * (value * (value * 6.0 - 15.0) + 10.0)
}

/// Column-major OpenGL-compatible perspective/view matrix measured from the
/// original 1280x720 trace. The native rendering path uses the same clip-space
/// depth interval for the submitted geometry (all cube vertices map to 0..1).
fn base_projection(size: RenderSize) -> [f32; 16] {
    let height = size.height.max(1) as f32;
    let aspect = size.width.max(1) as f32 / height;
    const FOCAL_LENGTH: f32 = 1.428_148; // cot(70 degrees / 2)
    const DEPTH_SCALE: f32 = 1.000_039_9;
    const DEPTH_TRANSLATION: f32 = 1.998_079_8;
    [
        -FOCAL_LENGTH / aspect,
        0.0,
        0.0,
        0.0,
        0.0,
        FOCAL_LENGTH,
        0.0,
        0.0,
        0.0,
        0.0,
        DEPTH_SCALE,
        1.0,
        0.0,
        0.0,
        DEPTH_TRANSLATION,
        2.0,
    ]
}

fn rotation_x(angle: f32) -> [f32; 16] {
    let (sin, cos) = angle.sin_cos();
    [
        1.0, 0.0, 0.0, 0.0, 0.0, cos, sin, 0.0, 0.0, -sin, cos, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

fn rotation_y(angle: f32) -> [f32; 16] {
    let (sin, cos) = angle.sin_cos();
    [
        cos, 0.0, -sin, 0.0, 0.0, 1.0, 0.0, 0.0, sin, 0.0, cos, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

/// Multiplies two column-major 4×4 matrices.
fn multiply(left: [f32; 16], right: [f32; 16]) -> [f32; 16] {
    let mut result = [0.0; 16];
    for column in 0..4 {
        for row in 0..4 {
            result[column * 4 + row] = (0..4)
                .map(|index| left[index * 4 + row] * right[column * 4 + index])
                .sum();
        }
    }
    result
}

// mat4x4<f32> (64 B) + five vec4<f32> offsets (80 B) + atlas width/padding.
const UNIFORM_BYTES: usize = 160;

fn uniform_bytes(
    matrix: [f32; 16],
    offsets: &[[f32; 2]; VISIBLE_FACE_COUNT],
    atlas_width: f32,
) -> [u8; UNIFORM_BYTES] {
    let mut bytes = [0; UNIFORM_BYTES];
    write_f32s(&mut bytes[0..64], &matrix);
    for (index, offset) in offsets.iter().enumerate() {
        let start = 64 + index * 16;
        write_f32s(&mut bytes[start..start + 8], offset);
    }
    write_f32s(&mut bytes[144..148], &[atlas_width]);
    bytes
}

fn write_f32s(destination: &mut [u8], values: &[f32]) {
    for (index, value) in values.iter().enumerate() {
        destination[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
}

const LOAD_CUBE_SHADER: &str = r#"
struct ModuleUniforms {
    mvp: mat4x4<f32>,
    slot_offsets: array<vec4<f32>, 5>,
    atlas_width: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex: vec2<f32>,
    @location(1) @interpolate(flat) slot: u32,
};

@group(0) @binding(0) var<uniform> uniforms: ModuleUniforms;
@group(0) @binding(1) var block_atlas: texture_2d<f32>;
@group(0) @binding(2) var block_sampler: sampler;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // Exact module=0 WebGL VBO: five faces, six vertices each.
    let positions = array<vec3<f32>, 30>(
        vec3<f32>(-0.5,-0.5,-0.5), vec3<f32>( 0.5,-0.5,-0.5), vec3<f32>(-0.5, 0.5,-0.5),
        vec3<f32>( 0.5,-0.5,-0.5), vec3<f32>( 0.5, 0.5,-0.5), vec3<f32>(-0.5, 0.5,-0.5),
        vec3<f32>(-0.5, 0.5,-0.5), vec3<f32>( 0.5, 0.5,-0.5), vec3<f32>(-0.5, 0.5, 0.5),
        vec3<f32>( 0.5, 0.5,-0.5), vec3<f32>( 0.5, 0.5, 0.5), vec3<f32>(-0.5, 0.5, 0.5),
        vec3<f32>(-0.5,-0.5, 0.5), vec3<f32>( 0.5,-0.5, 0.5), vec3<f32>(-0.5,-0.5,-0.5),
        vec3<f32>( 0.5,-0.5, 0.5), vec3<f32>( 0.5,-0.5,-0.5), vec3<f32>(-0.5,-0.5,-0.5),
        vec3<f32>(-0.5,-0.5, 0.5), vec3<f32>(-0.5,-0.5,-0.5), vec3<f32>(-0.5, 0.5, 0.5),
        vec3<f32>(-0.5,-0.5,-0.5), vec3<f32>(-0.5, 0.5,-0.5), vec3<f32>(-0.5, 0.5, 0.5),
        vec3<f32>( 0.5,-0.5,-0.5), vec3<f32>( 0.5,-0.5, 0.5), vec3<f32>( 0.5, 0.5,-0.5),
        vec3<f32>( 0.5,-0.5, 0.5), vec3<f32>( 0.5, 0.5, 0.5), vec3<f32>( 0.5, 0.5,-0.5),
    );
    let texcoords = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );

    var output: VertexOutput;
    output.position = uniforms.mvp * vec4<f32>(positions[vertex_index], 1.0);
    output.tex = texcoords[vertex_index % 6u];
    output.slot = vertex_index / 6u;
    return output;
}

fn srgb_to_linear(channel: f32) -> f32 {
    if (channel <= 0.04045) {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

fn web_color(input: VertexOutput) -> vec4<f32> {
    let slot = uniforms.slot_offsets[input.slot].xy;
    let tex = (slot + vec2<f32>(1.0 - input.tex.x, 1.0 - input.tex.y)) / uniforms.atlas_width;
    return textureSample(block_atlas, block_sampler, tex);
}

@fragment
fn fs_srgb(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = web_color(input);
    return vec4<f32>(
        vec3<f32>(
            srgb_to_linear(color.r),
            srgb_to_linear(color.g),
            srgb_to_linear(color.b),
        ),
        color.a,
    );
}

@fragment
fn fs_unorm(input: VertexOutput) -> @location(0) vec4<f32> {
    return web_color(input);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smootherstep_has_original_turn_checkpoints() {
        assert_eq!(smootherstep(0.0), 0.0);
        assert_eq!(smootherstep(0.5), 0.5);
        assert_eq!(smootherstep(1.0), 1.0);
    }

    #[test]
    fn transition_moves_current_directional_face_to_front() {
        let mut state = CubeState::new(32, 463, [7; 32]);
        state.offsets = [
            [0.0, 0.0],
            [1.0, 0.0],
            [2.0, 0.0],
            [3.0, 0.0],
            [4.0, 0.0],
            [5.0, 0.0],
        ];
        state.direction = 3;
        state.advance(TURN_DURATION_SECONDS);
        assert_eq!(state.offsets[0], [3.0, 0.0]);
        assert_ne!(state.offsets[3], [3.0, 0.0]);
        assert!((1..=4).contains(&state.direction));
        assert_eq!(state.offsets[5], [5.0, 0.0]);
    }

    #[test]
    fn offset_selection_stays_inside_manifest_range() {
        let mut random = Hc128Rng::from_seed([42; 32]);
        for _ in 0..10_000 {
            let offset = random_offset(&mut random, 32, 463);
            let id = offset[1] as u32 * 32 + offset[0] as u32;
            assert!(id < 463);
        }
    }

    #[test]
    fn uniform_layout_has_wgsl_uniform_alignment() {
        let bytes = uniform_bytes([1.0; 16], &[[2.0, 3.0]; VISIBLE_FACE_COUNT], 32.0);
        assert_eq!(&bytes[0..4], &1.0f32.to_ne_bytes());
        assert_eq!(&bytes[64..68], &2.0f32.to_ne_bytes());
        assert_eq!(&bytes[68..72], &3.0f32.to_ne_bytes());
        assert_eq!(&bytes[144..148], &32.0f32.to_ne_bytes());
    }
}
