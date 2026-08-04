use std::error::Error;

use rand::{RngCore, SeedableRng};
use rand_hc::Hc128Rng;

use super::{FrameInfo, Module, RenderContext, RenderSize};

const CREEPER_RESOURCE: &str = "creeper.png";
const CREEPER_DIMENSION: u32 = 8;
const INSTANCE_COUNT: u32 = 512;
const MAX_DISTANCE: f32 = 500.0;
const SPEED: f32 = 20.0;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const UNIFORM_BYTES: usize = 32;

/// Native wgpu implementation of Web module=12 (`creeper`).
///
/// Each of the original module's 512 sprites derives its fixed target and
/// phase from a single launch seed plus its instance index.  The native module
/// retains that shader-side model and uses the same HC-128 RNG core as the
/// Web build. Native entropy still prevents reproducing one particular Web
/// launch's seed.
pub struct CreeperModule {
    pipeline: Option<wgpu::RenderPipeline>,
    bind_group: Option<wgpu::BindGroup>,
    uniforms: Option<wgpu::Buffer>,
    depth: Option<DepthTarget>,
    seed: f32,
}

impl Default for CreeperModule {
    fn default() -> Self {
        Self {
            pipeline: None,
            bind_group: None,
            uniforms: None,
            depth: None,
            seed: random_seed(),
        }
    }
}

impl Module for CreeperModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let creeper = crate::resources::load_rgba_png(CREEPER_RESOURCE)?;
        if creeper.dimensions() != (CREEPER_DIMENSION, CREEPER_DIMENSION) {
            return Err(format!(
                "{CREEPER_RESOURCE} must be {CREEPER_DIMENSION}x{CREEPER_DIMENSION}, got {}x{}",
                creeper.width(),
                creeper.height()
            )
            .into());
        }

        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("creeper texture"),
            size: wgpu::Extent3d {
                width: CREEPER_DIMENSION,
                height: CREEPER_DIMENSION,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // The Web module uploads decoded pixel bytes directly to WebGL;
            // use a non-sRGB sampled texture for the same numeric samples.
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
            creeper.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(CREEPER_DIMENSION * 4),
                rows_per_image: Some(CREEPER_DIMENSION),
            },
            texture.size(),
        );
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("creeper sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let bind_group_layout =
            context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("creeper bind group layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::VERTEX,
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
                    label: Some("creeper pipeline layout"),
                    bind_group_layouts: &[Some(&bind_group_layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("creeper shader"),
                source: wgpu::ShaderSource::Wgsl(CREEPER_SHADER.into()),
            });
        let uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("creeper uniforms"),
            size: UNIFORM_BYTES as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        self.bind_group = Some(
            context
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("creeper bind group"),
                    layout: &bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniforms.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&texture_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&sampler),
                        },
                    ],
                }),
        );
        self.uniforms = Some(uniforms);
        self.pipeline = Some(context.device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("creeper pipeline"),
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
                    targets: &[Some(context.surface_format.into())],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    // The uploaded module-12 quad is CCW in clip space.
                    // Unlike module 0, this path has no X-mirroring MVP.
                    front_face: wgpu::FrontFace::Ccw,
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
            .expect("CreeperModule was not initialized");
        let bind_group = self
            .bind_group
            .as_ref()
            .expect("CreeperModule was not initialized");
        let uniforms = self
            .uniforms
            .as_ref()
            .expect("CreeperModule was not initialized");
        let depth = self.depth.as_ref().expect("CreeperModule was not resized");
        context.queue.write_buffer(
            uniforms,
            0,
            &uniform_bytes(
                frame.elapsed.as_secs_f32(),
                self.seed,
                frame.size.width.max(1) as f32,
                frame.size.height.max(1) as f32,
            ),
        );

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("creeper module"),
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
        pass.draw(0..6, 0..INSTANCE_COUNT);
    }
}

struct DepthTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl DepthTarget {
    fn new(device: &wgpu::Device, size: RenderSize) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("creeper depth texture"),
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

/// Use the Web build's rand 0.6 HC-128 core. The 32-byte native entropy seed
/// cannot reproduce a browser launch, but `next_u32() >> 8` uses the same
/// 24-bit `[0, 1)` conversion captured from module=12's WASM setup path.
fn random_seed() -> f32 {
    let mut entropy = [0; 32];
    rand::thread_rng().fill_bytes(&mut entropy);
    let mut random = Hc128Rng::from_seed(entropy);
    (random.next_u32() >> 8) as f32 / 16_777_216.0
}

fn uniform_bytes(time: f32, seed: f32, width: f32, height: f32) -> [u8; UNIFORM_BYTES] {
    // WGSL layout: four f32 values, viewport vec2<f32>, speed, padding.
    let mut bytes = [0; UNIFORM_BYTES];
    for (index, value) in [time, seed, height / 480.0, MAX_DISTANCE]
        .into_iter()
        .enumerate()
    {
        bytes[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
    for (index, value) in [width, height, SPEED].into_iter().enumerate() {
        let offset = 16 + index * 4;
        bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

const CREEPER_SHADER: &str = r#"
struct ModuleUniforms {
    time: f32,
    seed: f32,
    viewport_scale: f32,
    max_distance: f32,
    viewport: vec2<f32>,
    speed: f32,
    _padding: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) dim: f32,
};

@group(0) @binding(0) var<uniform> uniforms: ModuleUniforms;
@group(0) @binding(1) var creeper_texture: texture_2d<f32>;
@group(0) @binding(2) var creeper_sampler: sampler;

fn rand(value: f32) -> f32 {
    return fract(sin(value * 12.9898 + uniforms.seed) * 43758.5453);
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    // Exact module=12 WebGL VBO positions and texture coordinates.
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(-32.0, -32.0),
        vec2<f32>(32.0, -32.0),
        vec2<f32>(-32.0, 32.0),
        vec2<f32>(32.0, -32.0),
        vec2<f32>(32.0, 32.0),
        vec2<f32>(-32.0, 32.0),
    );
    let texture_coordinates = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 0.0),
    );

    let id = f32(instance_index);
    let time_offset = rand(id) * 100.0;
    let target_position = vec2<f32>(
        uniforms.viewport.x * (rand(21.0 * id) - 0.5),
        uniforms.viewport.y * (rand(55.0 * id) - 0.5),
    );
    let period = 30.0 - clamp(uniforms.speed, 0.0, 29.0);
    let scale = fract((uniforms.time + time_offset) / period);
    let distance = uniforms.max_distance * (1.0 - scale);
    let projected = target_position * (uniforms.max_distance / 2.0) / distance;
    let view = uniforms.viewport / 2.0
        + projected
        + positions[vertex_index] * pow(scale, 3.0) * uniforms.viewport_scale;

    var output: VertexOutput;
    output.position = vec4<f32>(
        view.x * 2.0 / uniforms.viewport.x - 1.0,
        view.y * 2.0 / uniforms.viewport.y - 1.0,
        // OpenGL maps the original clip Z (-scale/4) to [0, 1].
        0.5 - scale / 8.0,
        1.0,
    );
    output.uv = texture_coordinates[vertex_index];
    output.dim = scale;
    return output;
}

fn srgb_to_linear(channel: f32) -> f32 {
    if (channel <= 0.04045) {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // WebGL uploads the decoded PNG as a linear RGBA8 texture and its legacy
    // drawing buffer presents the shader's numeric output as sRGB. wgpu picks
    // an sRGB Wayland surface, whose render target instead encodes fragment
    // output. Convert the exact WebGL numeric RGB result back to linear before
    // that target conversion, keeping the observed RGB * scale^4 appearance.
    let sampled = textureSample(creeper_texture, creeper_sampler, input.uv);
    let dim = pow(input.dim, 4.0);
    let webgl_rgb = sampled.rgb * dim;
    return vec4<f32>(
        vec3<f32>(
            srgb_to_linear(webgl_rgb.r),
            srgb_to_linear(webgl_rgb.g),
            srgb_to_linear(webgl_rgb.b),
        ),
        // The target is a wallpaper, not a translucent overlay. The WebGL
        // shader's alpha is useful inside its own canvas backing store, but
        // exposing it to Wayland reveals the previous wallpaper.
        1.0,
    );
}
"#;

#[cfg(test)]
mod tests {
    use super::{MAX_DISTANCE, UNIFORM_BYTES, uniform_bytes};

    #[test]
    fn uniforms_follow_wgsl_layout() {
        let bytes = uniform_bytes(1.5, 0.25, 1920.0, 1080.0);
        assert_eq!(bytes.len(), UNIFORM_BYTES);
        assert_eq!(&bytes[0..4], &1.5f32.to_ne_bytes());
        assert_eq!(&bytes[4..8], &0.25f32.to_ne_bytes());
        assert_eq!(&bytes[8..12], &2.25f32.to_ne_bytes());
        assert_eq!(&bytes[12..16], &MAX_DISTANCE.to_ne_bytes());
        assert_eq!(&bytes[16..20], &1920.0f32.to_ne_bytes());
        assert_eq!(&bytes[20..24], &1080.0f32.to_ne_bytes());
        assert_eq!(&bytes[24..28], &20.0f32.to_ne_bytes());
    }
}
