use std::error::Error;

use rand::RngCore;
use wasmi::{Engine, ExternType, Linker, Memory, Module as WasmModule, Store, Val};

use super::{FrameInfo, Module, RenderContext, RenderSize, web_surface_fragment_entry};

const GRID_SIZE: u32 = 16;
const INSTANCE_COUNT: u32 = GRID_SIZE * GRID_SIZE;
const TEXTURE_BYTES: usize = (GRID_SIZE * GRID_SIZE * 4) as usize;
const TICK_SECONDS: f32 = 0.05;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Native names for Web modules 4 and 5.
///
/// The names are intentionally a CLI/API convenience: the Web ZIP has no
/// named fluid texture from which an official material identity can be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlphaFluidVariant {
    /// Web `module=4`: blue, two-field fluid simulation.
    Water,
    /// Web `module=5`: red/yellow, four-neighbour fluid simulation.
    Lava,
}

/// Native wgpu implementation of Web modules 4 and 5 (`alpha_fluids`).
///
/// The module has a still top material and a vertically scrolling flowing side
/// material. Both are separate 16x16 RGBA textures generated at runtime; they
/// are not substituted with a flat clear colour even when their initial texels
/// are almost uniform.
pub struct AlphaFluidsModule {
    variant: AlphaFluidVariant,
    pipeline: Option<wgpu::RenderPipeline>,
    bind_group: Option<wgpu::BindGroup>,
    uniforms: Option<wgpu::Buffer>,
    textures: Option<FluidTextures>,
    depth: Option<DepthTarget>,
    // Both variants execute the original Web update function through wasmi.
    water_runtime: Option<OriginalWaterRuntime>,
    lava_runtime: Option<OriginalLavaRuntime>,
    runtime_texels: Option<([u8; TEXTURE_BYTES], [u8; TEXTURE_BYTES])>,
    last_tick: Option<u64>,
    textures_dirty: bool,
}

impl AlphaFluidsModule {
    pub fn new(variant: AlphaFluidVariant) -> Self {
        Self {
            variant,
            pipeline: None,
            bind_group: None,
            uniforms: None,
            textures: None,
            depth: None,
            water_runtime: None,
            lava_runtime: None,
            runtime_texels: None,
            last_tick: None,
            textures_dirty: false,
        }
    }

    fn upload_textures(&mut self, context: &RenderContext<'_>) {
        let textures = self
            .textures
            .as_ref()
            .expect("AlphaFluidsModule was not initialized");
        let (still, flowing) = self
            .runtime_texels
            .as_ref()
            .expect("original Web fluid texels were not initialized");
        write_texture(context.queue, &textures.still, still);
        write_texture(context.queue, &textures.flowing, flowing);
        self.textures_dirty = false;
    }
}

impl Module for AlphaFluidsModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let still = create_fluid_texture(context.device, "alpha-fluids still texture");
        let flowing = create_fluid_texture(context.device, "alpha-fluids flowing texture");
        let still_view = still.create_view(&wgpu::TextureViewDescriptor::default());
        let flowing_view = flowing.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("alpha-fluids nearest sampler"),
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
                    label: Some("alpha-fluids bind group layout"),
                    entries: &[
                        uniform_binding(0, wgpu::ShaderStages::VERTEX),
                        texture_binding(1),
                        texture_binding(2),
                        sampler_binding(3),
                    ],
                });
        let pipeline_layout =
            context
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("alpha-fluids pipeline layout"),
                    bind_group_layouts: &[Some(&bind_group_layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("alpha-fluids shader"),
                source: wgpu::ShaderSource::Wgsl(ALPHA_FLUIDS_SHADER.into()),
            });
        let uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("alpha-fluids MVP uniform"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("alpha-fluids bind group"),
                layout: &bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&still_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&flowing_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            });

        self.pipeline = Some(context.device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("alpha-fluids pipeline"),
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
                    targets: &[Some(wgpu::ColorTargetState {
                        format: context.surface_format,
                        // The recovered WebGL path never enables BLEND. The
                        // fragment shader returns alpha=1, so this must not
                        // become a Wayland alpha-blended overlay.
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    // The recovered top triangle remains CCW after this
                    // module's MVP: e.g. at 16:9 its first triangle projects
                    // to (0, .074), (-.101, -.053), (.152, .017), whose
                    // signed area is positive. Unlike module 0, this MVP has
                    // no X mirror, so it must use WebGPU's CCW front face.
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
        self.bind_group = Some(bind_group);
        self.uniforms = Some(uniforms);
        self.textures = Some(FluidTextures { still, flowing });
        log::debug!(
            target: "minecraft_plus_wayland::wasm",
            "initializing alpha-fluid WASM runtime: variant={:?}",
            self.variant,
        );
        match self.variant {
            AlphaFluidVariant::Water => {
                self.water_runtime = Some(OriginalWaterRuntime::new()?);
            }
            AlphaFluidVariant::Lava => {
                self.lava_runtime = Some(OriginalLavaRuntime::new()?);
            }
        }
        // The first frame's update performs the first and only initial WASM
        // step. No synthetic raster is uploaded while runtime_texels is None.
        Ok(())
    }

    fn resize(&mut self, context: &RenderContext<'_>, size: RenderSize) {
        self.depth = Some(DepthTarget::new(context.device, size));
    }

    fn update(&mut self, frame: FrameInfo) {
        let tick = (frame.elapsed.as_secs_f32().max(0.0) / TICK_SECONDS).floor() as u64;
        if !advance_observed_bucket(&mut self.last_tick, tick) {
            return;
        }

        // Web advances at most once when it observes a new 50 ms bucket. A
        // compositor time jump therefore performs one current update and
        // discards every unobserved intermediate bucket.
        log::trace!(
            target: "minecraft_plus_wayland::wasm",
            "stepping alpha-fluid WASM runtime: variant={:?}, observed_bucket={tick}",
            self.variant,
        );
        let texels = match self.variant {
            AlphaFluidVariant::Water => self
                .water_runtime
                .as_mut()
                .expect("Water runtime was not initialized")
                .tick()
                .expect("original Web water step failed before producing textures"),
            AlphaFluidVariant::Lava => self
                .lava_runtime
                .as_mut()
                .expect("Lava runtime was not initialized")
                .tick()
                .expect("original Web lava step failed before producing textures"),
        };
        self.runtime_texels = Some(texels);
        self.textures_dirty = true;
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
        if self.textures_dirty {
            self.upload_textures(context);
        }
        let pipeline = self
            .pipeline
            .as_ref()
            .expect("AlphaFluidsModule was not initialized");
        let bind_group = self
            .bind_group
            .as_ref()
            .expect("AlphaFluidsModule was not initialized");
        let uniforms = self
            .uniforms
            .as_ref()
            .expect("AlphaFluidsModule was not initialized");
        let depth = self
            .depth
            .as_ref()
            .expect("AlphaFluidsModule was not resized");
        context
            .queue
            .write_buffer(uniforms, 0, &matrix_bytes(alpha_fluids_mvp(frame.size)));

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("alpha-fluids module"),
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
        // Equivalent to the Web loop that sets instanceId=0..255 and calls
        // drawArrays(GL_TRIANGLES, 0, 18) for every cell.
        pass.draw(0..18, 0..INSTANCE_COUNT);
    }
}

struct FluidTextures {
    still: wgpu::Texture,
    flowing: wgpu::Texture,
}

struct DepthTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl DepthTarget {
    fn new(device: &wgpu::Device, size: RenderSize) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("alpha-fluids depth texture"),
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

fn create_fluid_texture(device: &wgpu::Device, label: &'static str) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: GRID_SIZE,
            height: GRID_SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        // WebGL receives raw RGBA bytes, rather than an sRGB-tagged texture.
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_texture(queue: &wgpu::Queue, texture: &wgpu::Texture, bytes: &[u8; TEXTURE_BYTES]) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(GRID_SIZE * 4),
            rows_per_image: Some(GRID_SIZE),
        },
        texture.size(),
    );
}

fn advance_observed_bucket(last_tick: &mut Option<u64>, tick: u64) -> bool {
    if last_tick.is_some_and(|last_tick| tick <= last_tick) {
        return false;
    }
    *last_tick = Some(tick);
    true
}

/// The captured column-major WebGL matrix, generalized for the native surface
/// size. `vs_main` maps its OpenGL [-w,w] Z coordinate to WebGPU [0,w].
fn alpha_fluids_mvp(size: RenderSize) -> [f32; 16] {
    let width = size.width.max(1) as f32;
    let height = size.height.max(1) as f32;
    let aspect = width / height;
    let sx = (1.0 / aspect).max(1.0) / 7.0;
    let sy = aspect.max(1.0) / 7.0;
    let sz = 1.0 / 7.0;
    [
        sx * std::f32::consts::FRAC_1_SQRT_2,
        -sy * 0.5,
        -sz * std::f32::consts::FRAC_1_SQRT_2,
        0.0,
        0.0,
        sy * std::f32::consts::FRAC_1_SQRT_2,
        -sz * std::f32::consts::FRAC_1_SQRT_2,
        0.0,
        -sx * std::f32::consts::FRAC_1_SQRT_2,
        -sy * 0.5,
        -sz * std::f32::consts::FRAC_1_SQRT_2,
        0.0,
        0.0,
        -1.0 / 7.0,
        0.0,
        1.0,
    ]
}

fn matrix_bytes(matrix: [f32; 16]) -> [u8; 64] {
    let mut bytes = [0; 64];
    for (index, value) in matrix.into_iter().enumerate() {
        bytes[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

const ALPHA_FLUIDS_SHADER: &str = r#"
struct Uniforms {
    mvp: mat4x4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) flowing: f32,
    @location(2) light: f32,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var still_texture: texture_2d<f32>;
@group(0) @binding(2) var flowing_texture: texture_2d<f32>;
@group(0) @binding(3) var fluid_sampler: sampler;

fn srgb_to_linear(channel: f32) -> f32 {
    if (channel <= 0.04045) {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    // Exact position / u / v / texture-selector / fake-light VBO recovered
    // from the Web module. First six vertices are the still top; the next
    // twelve are the two flowing, downward-v side faces.
    let positions = array<vec3<f32>, 18>(
        vec3<f32>(-0.5,  0.5, -0.5), vec3<f32>(-0.5,  0.5,  0.5), vec3<f32>( 0.5,  0.5, -0.5),
        vec3<f32>( 0.5,  0.5, -0.5), vec3<f32>(-0.5,  0.5,  0.5), vec3<f32>( 0.5,  0.5,  0.5),
        vec3<f32>(-0.5, -0.5,  0.5), vec3<f32>( 0.5, -0.5,  0.5), vec3<f32>(-0.5,  0.5,  0.5),
        vec3<f32>( 0.5, -0.5,  0.5), vec3<f32>( 0.5,  0.5,  0.5), vec3<f32>(-0.5,  0.5,  0.5),
        vec3<f32>( 0.5, -0.5, -0.5), vec3<f32>( 0.5,  0.5, -0.5), vec3<f32>( 0.5, -0.5,  0.5),
        vec3<f32>( 0.5, -0.5,  0.5), vec3<f32>( 0.5,  0.5, -0.5), vec3<f32>( 0.5,  0.5,  0.5),
    );
    let texels = array<vec3<f32>, 18>(
        vec3<f32>(0.0, 0.0, 0.0), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0),
        vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 1.0, 0.0),
        vec3<f32>(0.0, 1.0, 1.0), vec3<f32>(1.0, 1.0, 1.0), vec3<f32>(0.0, 0.0, 1.0),
        vec3<f32>(1.0, 1.0, 1.0), vec3<f32>(1.0, 0.0, 1.0), vec3<f32>(0.0, 0.0, 1.0),
        vec3<f32>(0.0, 1.0, 1.0), vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(1.0, 1.0, 1.0),
        vec3<f32>(1.0, 1.0, 1.0), vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(1.0, 0.0, 1.0),
    );
    let lights = array<f32, 18>(
        1.0, 1.0, 1.0, 1.0, 1.0, 1.0,
        0.9, 0.9, 0.9, 0.9, 0.9, 0.9,
        0.8, 0.8, 0.8, 0.8, 0.8, 0.8,
    );
    let row = instance_index / 16u;
    let column = instance_index - row * 16u;
    let displacement = vec3<f32>(
        f32(row) - 8.0,
        -(f32(row) - 8.0 + f32(column) - 8.0),
        f32(column) - 8.0,
    );
    let texel = texels[vertex_index];
    var output: VertexOutput;
    let webgl_position = uniforms.mvp * vec4<f32>(positions[vertex_index] + displacement, 1.0);
    // WebGL clips Z in [-w,w]; WebGPU uses [0,w]. Preserve its depth order.
    output.position = vec4<f32>(webgl_position.xy, (webgl_position.z + webgl_position.w) * 0.5, webgl_position.w);
    output.uv = texel.xy;
    output.flowing = texel.z;
    output.light = lights[vertex_index];
    return output;
}

fn web_color(input: VertexOutput) -> vec4<f32> {
    let still = textureSample(still_texture, fluid_sampler, input.uv);
    let flowing = textureSample(flowing_texture, fluid_sampler, input.uv);
    let sample = select(still, flowing, input.flowing > 0.0);
    return vec4<f32>(sample.rgb * sample.a * input.light, 1.0);
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
        1.0,
    );
}

@fragment
fn fs_unorm(input: VertexOutput) -> @location(0) vec4<f32> {
    return web_color(input);
}
"#;

#[cfg(test)]
mod tests {
    use super::{GRID_SIZE, RenderSize, advance_observed_bucket, alpha_fluids_mvp, matrix_bytes};

    #[test]
    fn observed_buckets_step_once_without_catching_up() {
        let mut last_tick = None;
        let observed = [0, 0, 1, 10_000, 10_000, 75_000];
        let steps = observed
            .into_iter()
            .filter(|tick| advance_observed_bucket(&mut last_tick, *tick))
            .count();

        assert_eq!(steps, 4);
        assert_eq!(last_tick, Some(75_000));
    }

    #[test]
    fn mvp_matches_the_captured_widescreen_values() {
        let matrix = alpha_fluids_mvp(RenderSize {
            width: 1280,
            height: 720,
        });
        assert!((matrix[0] - 0.101_015_25).abs() < 0.000_001);
        assert!((matrix[5] - 0.179_582_69).abs() < 0.000_001);
        assert!((matrix[13] + 1.0 / 7.0).abs() < 0.000_001);
        assert_eq!(matrix_bytes(matrix).len(), 64);
        assert_eq!(GRID_SIZE, 16);
    }
}

const STATE_BYTES: usize = 4_104;
const PIXEL_BYTES: usize = 1_024;
const LAVA_STEP_FUNCTION: u32 = 143;
const LAVA_STEP_EXPORT: &str = "__minecraft_plus_lava_step";
const WATER_STEP_FUNCTION: u32 = 116;
const WATER_STEP_EXPORT: &str = "__minecraft_plus_water_step";
const WEBGL_TEX_IMAGE_2D: &str = "__wbg_texImage2D_1c4b87cd146e7590";
const GET_RANDOM_VALUES: &str = "__wbg_getRandomValues_1ef11e888e5228e9";
const RANDOM_FILL_SYNC: &str = "__wbg_randomFillSync_1b52c8482374c55b";
const GL_TEXTURE_2D: i32 = 3_553;
const GL_RGBA: i32 = 6_408;
const GL_UNSIGNED_BYTE: i32 = 5_121;

#[derive(Default)]
struct RuntimeState {
    uploads: Vec<[u8; PIXEL_BYTES]>,
    // Tests can replace system entropy with a fixed byte without changing the
    // production path or introducing a second pseudo-random implementation.
    #[cfg(test)]
    fixed_entropy: Option<u8>,
    #[cfg(test)]
    entropy_writes: Vec<(usize, Vec<u8>)>,
}

impl RuntimeState {
    fn entropy(&mut self, length: usize) -> Vec<u8> {
        let mut bytes = vec![0; length];
        #[cfg(test)]
        if let Some(byte) = self.fixed_entropy {
            bytes.fill(byte);
            return bytes;
        }
        rand::thread_rng().fill_bytes(&mut bytes);
        bytes
    }
}

/// Executes the original Web `func[143]` lava update directly in wasmi.
///
/// The Web module does not export that internal function. The loader adds one
/// export record only; all code, data, memory and internal call indices remain
/// byte-for-byte the original module.
pub(super) struct OriginalLavaRuntime {
    store: Store<RuntimeState>,
    memory: Memory,
    step: wasmi::TypedFunc<(i32, i32, i32), ()>,
    still_state: i32,
    flowing_state: i32,
    still_pixels: i32,
    flowing_pixels: i32,
}

impl OriginalLavaRuntime {
    pub(super) fn new() -> Result<Self, Box<dyn Error>> {
        Self::new_with_state(RuntimeState::default())
    }

    fn new_with_state(runtime_state: RuntimeState) -> Result<Self, Box<dyn Error>> {
        let wasm = crate::resources::load_web_wasm()?;
        let wasm = export_internal_function(&wasm, LAVA_STEP_EXPORT, LAVA_STEP_FUNCTION)?;

        let engine = Engine::default();
        let module = WasmModule::new(&engine, &wasm[..])?;
        let mut linker = Linker::<RuntimeState>::new(&engine);
        define_web_import_stubs(&module, &mut linker)?;
        let mut store = Store::new(&engine, runtime_state);
        let instance = linker.instantiate_and_start(&mut store, &module)?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or("original Web WASM did not export memory")?;
        let malloc = instance.get_typed_func::<i32, i32>(&store, "__wbindgen_malloc")?;
        let step = instance.get_typed_func::<(i32, i32, i32), ()>(&store, LAVA_STEP_EXPORT)?;
        let still_state = malloc.call(&mut store, STATE_BYTES as i32)?;
        let flowing_state = malloc.call(&mut store, STATE_BYTES as i32)?;
        let still_pixels = malloc.call(&mut store, PIXEL_BYTES as i32)?;
        let flowing_pixels = malloc.call(&mut store, PIXEL_BYTES as i32)?;
        memory.write(&mut store, still_state as usize, &vec![0; STATE_BYTES])?;
        memory.write(&mut store, flowing_state as usize, &vec![0; STATE_BYTES])?;
        // The Web state at p0 + 4100 gates its / -3 scrolling counter. It is
        // clear for still and set for flowing.
        memory.write(&mut store, (flowing_state as usize) + 4100, &[1])?;
        log::debug!(
            target: "minecraft_plus_wayland::wasm",
            "lava WASM runtime ready: function_index={LAVA_STEP_FUNCTION}, export={LAVA_STEP_EXPORT}, \
             still_state={still_state}, flowing_state={flowing_state}, still_pixels={still_pixels}, \
             flowing_pixels={flowing_pixels}",
        );

        Ok(Self {
            store,
            memory,
            step,
            still_state,
            flowing_state,
            still_pixels,
            flowing_pixels,
        })
    }

    pub(super) fn tick(
        &mut self,
    ) -> Result<([u8; PIXEL_BYTES], [u8; PIXEL_BYTES]), Box<dyn Error>> {
        log::trace!(
            target: "minecraft_plus_wayland::wasm",
            "calling lava WASM step twice for still/flowing rasters",
        );
        self.step.call(
            &mut self.store,
            (self.still_state, self.still_pixels, PIXEL_BYTES as i32),
        )?;
        self.step.call(
            &mut self.store,
            (self.flowing_state, self.flowing_pixels, PIXEL_BYTES as i32),
        )?;
        let mut still = [0; PIXEL_BYTES];
        let mut flowing = [0; PIXEL_BYTES];
        self.memory
            .read(&self.store, self.still_pixels as usize, &mut still)?;
        self.memory
            .read(&self.store, self.flowing_pixels as usize, &mut flowing)?;
        Ok((still, flowing))
    }
}

/// Executes the original Web `func[116]` water update and collects its two
/// `texImage2D` uploads from the original function's own WebGL call path.
pub(super) struct OriginalWaterRuntime {
    store: Store<RuntimeState>,
    step: wasmi::TypedFunc<(i32, f64), ()>,
    state: i32,
    tick: u64,
    last_uploads: Option<([u8; PIXEL_BYTES], [u8; PIXEL_BYTES])>,
}

impl OriginalWaterRuntime {
    pub(super) fn new() -> Result<Self, Box<dyn Error>> {
        Self::new_with_state(RuntimeState::default())
    }

    fn new_with_state(runtime_state: RuntimeState) -> Result<Self, Box<dyn Error>> {
        let wasm = crate::resources::load_web_wasm()?;
        let wasm = export_internal_function(&wasm, WATER_STEP_EXPORT, WATER_STEP_FUNCTION)?;
        let engine = Engine::default();
        let module = WasmModule::new(&engine, &wasm[..])?;
        let mut linker = Linker::<RuntimeState>::new(&engine);
        define_web_import_stubs(&module, &mut linker)?;
        let mut store = Store::new(&engine, runtime_state);
        let instance = linker.instantiate_and_start(&mut store, &module)?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or("original Web WASM did not export memory")?;
        let malloc = instance.get_typed_func::<i32, i32>(&store, "__wbindgen_malloc")?;
        let step = instance.get_typed_func::<(i32, f64), ()>(&store, WATER_STEP_EXPORT)?;
        // func[116] addresses fields as far as a + 8312. Reserve a little
        // extra, then supply its two 16x16 upload dimensions and shared byte
        // buffer exactly where the original struct expects them.
        let state = malloc.call(&mut store, 8_400)?;
        let pixels = malloc.call(&mut store, PIXEL_BYTES as i32)?;
        memory.write(&mut store, state as usize, &vec![0; 8_400])?;
        for (offset, value) in [
            (92, pixels),
            (100, PIXEL_BYTES as i32),
            // func[116] forwards field 104 as both internalformat and format,
            // followed by width and height from fields 108 and 112.
            (104, GL_RGBA),
            (108, GRID_SIZE as i32),
            (112, GRID_SIZE as i32),
        ] {
            memory.write(&mut store, state as usize + offset, &value.to_le_bytes())?;
        }
        log::debug!(
            target: "minecraft_plus_wayland::wasm",
            "water WASM runtime ready: function_index={WATER_STEP_FUNCTION}, export={WATER_STEP_EXPORT}, \
             state={state}, pixels={pixels}, raster={}x{} RGBA8",
            GRID_SIZE,
            GRID_SIZE,
        );
        Ok(Self {
            store,
            step,
            state,
            tick: 0,
            last_uploads: None,
        })
    }

    pub(super) fn tick(
        &mut self,
    ) -> Result<([u8; PIXEL_BYTES], [u8; PIXEL_BYTES]), Box<dyn Error>> {
        self.tick += 1;
        self.store.data_mut().uploads.clear();
        let wasm_time = self.tick as f64 * 0.05;
        log::trace!(
            target: "minecraft_plus_wayland::wasm",
            "calling water WASM step: internal_tick={}, time={wasm_time:.3}",
            self.tick,
        );
        self.step.call(&mut self.store, (self.state, wasm_time))?;
        let uploads = &self.store.data().uploads;
        match uploads.len() {
            0 if self.last_uploads.is_some() => {
                // func[116] gates texture writes by floor(time / .05). If
                // floating-point rounding repeats a bucket after a successful
                // capture, WebGL keeps the prior pair.
            }
            0 => {
                return Err(
                    "original Web water function produced no initial texture uploads".into(),
                );
            }
            2 => {
                if self.last_uploads.is_none() {
                    log::debug!(
                        target: "minecraft_plus_wayland::wasm",
                        "captured initial water WASM rasters: uploads=2, bytes_per_raster={PIXEL_BYTES}",
                    );
                }
                self.last_uploads = Some((uploads[0], uploads[1]));
            }
            count => {
                return Err(format!(
                    "original Web water function produced {count} texture uploads, expected 0 or 2"
                )
                .into());
            }
        }
        self.last_uploads
            .ok_or_else(|| "original Web water raster capture was not initialized".into())
    }
}

fn define_web_import_stubs(
    module: &WasmModule,
    linker: &mut Linker<RuntimeState>,
) -> Result<(), Box<dyn Error>> {
    #[derive(Clone, Copy)]
    enum ImportBehavior {
        Stub,
        CaptureUpload,
        FillEntropy,
    }

    for import in module.imports() {
        let ExternType::Func(function_type) = import.ty() else {
            return Err(format!(
                "unexpected non-function Web import {}::{}",
                import.module(),
                import.name()
            )
            .into());
        };
        let result_types = function_type.results().to_vec();
        let behavior = match import.name() {
            WEBGL_TEX_IMAGE_2D => ImportBehavior::CaptureUpload,
            GET_RANDOM_VALUES | RANDOM_FILL_SYNC => ImportBehavior::FillEntropy,
            _ => ImportBehavior::Stub,
        };
        let import_name = import.name().to_owned();
        if !matches!(behavior, ImportBehavior::Stub) {
            log::debug!(
                target: "minecraft_plus_wayland::wasm",
                "binding Web WASM host import: {}::{import_name}",
                import.module(),
            );
        }
        linker.func_new(
            import.module(),
            import.name(),
            function_type.clone(),
            move |mut caller, params, results| {
                match behavior {
                    ImportBehavior::Stub => {
                        // These values exist only to satisfy the original
                        // module's JS ABI. In particular, zero-valued handles
                        // are not treated as real JavaScript objects.
                    }
                    ImportBehavior::CaptureUpload => {
                        capture_texture_upload(&mut caller, params, &import_name)?;
                    }
                    ImportBehavior::FillEntropy => {
                        fill_guest_entropy(&mut caller, params, &import_name)?;
                    }
                }
                for (result, value_type) in results.iter_mut().zip(&result_types) {
                    *result = Val::default(*value_type);
                }
                Ok(())
            },
        )?;
    }
    Ok(())
}

fn capture_texture_upload(
    caller: &mut wasmi::Caller<'_, RuntimeState>,
    params: &[Val],
    import_name: &str,
) -> Result<(), wasmi::Error> {
    if params.len() != 11 {
        return Err(wasmi::Error::new(format!(
            "{import_name} received {} parameters, expected 11",
            params.len()
        )));
    }
    for (index, expected, label) in [
        (1, GL_TEXTURE_2D, "target"),
        (2, 0, "level"),
        (3, GL_RGBA, "internalformat"),
        (4, GRID_SIZE as i32, "width"),
        (5, GRID_SIZE as i32, "height"),
        (6, 0, "border"),
        (7, GL_RGBA, "format"),
        (8, GL_UNSIGNED_BYTE, "type"),
        (10, PIXEL_BYTES as i32, "length"),
    ] {
        let actual = i32_parameter(params, index, import_name)?;
        if actual != expected {
            return Err(wasmi::Error::new(format!(
                "{import_name} received invalid {label} {actual}, expected {expected}"
            )));
        }
    }

    let (memory, pointer, length) = checked_guest_range(caller, params, 9, 10, import_name)?;
    debug_assert_eq!(length, PIXEL_BYTES);
    let mut upload = [0; PIXEL_BYTES];
    memory.read(&*caller, pointer, &mut upload)?;
    log::trace!(
        target: "minecraft_plus_wayland::wasm",
        "captured WebGL texture upload from WASM: pointer={pointer}, length={length}, size={}x{}",
        GRID_SIZE,
        GRID_SIZE,
    );
    caller.data_mut().uploads.push(upload);
    Ok(())
}

fn fill_guest_entropy(
    caller: &mut wasmi::Caller<'_, RuntimeState>,
    params: &[Val],
    import_name: &str,
) -> Result<(), wasmi::Error> {
    if params.len() != 3 {
        return Err(wasmi::Error::new(format!(
            "{import_name} received {} parameters, expected 3",
            params.len()
        )));
    }
    // params[0] is an opaque wasm-bindgen JS handle. It is deliberately not
    // dereferenced or otherwise represented as a native object.
    let (memory, pointer, length) = checked_guest_range(caller, params, 1, 2, import_name)?;
    let bytes = caller.data_mut().entropy(length);
    memory.write(&mut *caller, pointer, &bytes)?;
    log::debug!(
        target: "minecraft_plus_wayland::wasm",
        "filled Web WASM entropy import: import={import_name}, pointer={pointer}, length={length}",
    );
    #[cfg(test)]
    caller.data_mut().entropy_writes.push((pointer, bytes));
    Ok(())
}

fn checked_guest_range(
    caller: &wasmi::Caller<'_, RuntimeState>,
    params: &[Val],
    pointer_index: usize,
    length_index: usize,
    import_name: &str,
) -> Result<(Memory, usize, usize), wasmi::Error> {
    let pointer = i32_parameter(params, pointer_index, import_name)?;
    let length = i32_parameter(params, length_index, import_name)?;
    let pointer = usize::try_from(pointer)
        .map_err(|_| wasmi::Error::new(format!("{import_name} received negative guest pointer")))?;
    let length = usize::try_from(length)
        .map_err(|_| wasmi::Error::new(format!("{import_name} received negative length")))?;
    let memory = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or_else(|| wasmi::Error::new(format!("{import_name} could not access guest memory")))?;
    let end = pointer
        .checked_add(length)
        .ok_or_else(|| wasmi::Error::new(format!("{import_name} guest range overflow")))?;
    if end > memory.data_size(caller) {
        return Err(wasmi::Error::new(format!(
            "{import_name} guest range {pointer}..{end} exceeds memory size {}",
            memory.data_size(caller)
        )));
    }
    Ok((memory, pointer, length))
}

fn i32_parameter(params: &[Val], index: usize, import_name: &str) -> Result<i32, wasmi::Error> {
    params
        .get(index)
        .and_then(Val::i32)
        .ok_or_else(|| wasmi::Error::new(format!("{import_name} parameter {index} is not i32")))
}

fn export_internal_function(
    wasm: &[u8],
    export_name: &str,
    function_index: u32,
) -> Result<Vec<u8>, Box<dyn Error>> {
    if wasm.get(..8) != Some(b"\0asm\x01\0\0\0") {
        return Err("original Web WASM has an invalid header".into());
    }
    let mut cursor = 8;
    while cursor < wasm.len() {
        let section_start = cursor;
        let section_id = wasm[cursor];
        cursor += 1;
        let (section_length, length_end) = read_uleb(wasm, cursor)?;
        let payload_start = length_end;
        let payload_end = payload_start
            .checked_add(section_length as usize)
            .ok_or("WASM section length overflow")?;
        if payload_end > wasm.len() {
            return Err("WASM section extends beyond file".into());
        }
        if section_id == 7 {
            let (export_count, count_end) = read_uleb(wasm, payload_start)?;
            let mut payload = Vec::with_capacity(section_length as usize + export_name.len() + 8);
            write_uleb(&mut payload, export_count + 1);
            payload.extend_from_slice(&wasm[count_end..payload_end]);
            write_uleb(&mut payload, export_name.len() as u32);
            payload.extend_from_slice(export_name.as_bytes());
            payload.push(0); // external kind: function
            write_uleb(&mut payload, function_index);

            let mut patched =
                Vec::with_capacity(wasm.len() + payload.len() - section_length as usize);
            patched.extend_from_slice(&wasm[..section_start]);
            patched.push(section_id);
            write_uleb(&mut patched, payload.len() as u32);
            patched.extend_from_slice(&payload);
            patched.extend_from_slice(&wasm[payload_end..]);
            log::debug!(
                target: "minecraft_plus_wayland::wasm",
                "patched Web WASM export section: export={export_name}, function_index={function_index}, \
                 original_bytes={}, patched_bytes={}",
                wasm.len(),
                patched.len(),
            );
            return Ok(patched);
        }
        cursor = payload_end;
    }
    Err("original Web WASM has no export section".into())
}

fn read_uleb(bytes: &[u8], mut cursor: usize) -> Result<(u32, usize), Box<dyn Error>> {
    let mut value = 0_u32;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(cursor).ok_or("truncated ULEB128 value")?;
        cursor += 1;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, cursor));
        }
    }
    Err("ULEB128 value is too large".into())
}

fn write_uleb(bytes: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        bytes.push(byte);
        if value == 0 {
            return;
        }
    }
}

#[cfg(test)]
mod wasm_runtime_tests {
    use super::{
        LAVA_STEP_EXPORT, LAVA_STEP_FUNCTION, OriginalLavaRuntime, OriginalWaterRuntime,
        PIXEL_BYTES, RuntimeState, export_internal_function,
    };

    #[test]
    fn internal_export_patch_preserves_the_original_and_adds_one_export() {
        let wasm = crate::resources::load_web_wasm().unwrap();
        let patched =
            export_internal_function(&wasm, LAVA_STEP_EXPORT, LAVA_STEP_FUNCTION).unwrap();
        assert!(patched.len() > wasm.len());
        assert!(
            patched
                .windows(LAVA_STEP_EXPORT.len())
                .any(|window| window == LAVA_STEP_EXPORT.as_bytes())
        );
    }

    #[test]
    fn original_lava_function_executes_and_uploads_two_rgba_rasters() {
        let mut runtime = OriginalLavaRuntime::new().unwrap();
        let (still, flowing) = runtime.tick().unwrap();
        assert_eq!(&still[..4], &[155, 0, 0, 255]);
        assert_eq!(&flowing[..4], &[155, 0, 0, 255]);
        let mut has_detail = false;
        for _ in 0..100 {
            let (still, flowing) = runtime.tick().unwrap();
            has_detail |= still
                .chunks_exact(4)
                .chain(flowing.chunks_exact(4))
                .any(|pixel| pixel != [155, 0, 0, 255]);
        }
        assert!(has_detail);
    }

    #[test]
    fn original_water_function_executes_and_captures_both_uploads() {
        let mut runtime = OriginalWaterRuntime::new().unwrap();
        assert!(runtime.last_uploads.is_none());
        let (still, flowing) = runtime.tick().unwrap();
        assert!(runtime.last_uploads.is_some());
        assert_eq!(&still[..4], &[32, 50, 146, 255]);
        assert_eq!(&flowing[..4], &[32, 50, 146, 255]);

        // Repeating the same internal time bucket yields no new WebGL upload,
        // but is valid after the initial pair has been captured.
        runtime.tick -= 1;
        let repeated = runtime.tick().unwrap();
        assert_eq!(repeated, (still, flowing));

        for _ in 0..32 {
            let (still, flowing) = runtime.tick().unwrap();
            assert_eq!(still.len(), PIXEL_BYTES);
            assert_eq!(flowing.len(), PIXEL_BYTES);
        }
    }

    #[test]
    fn wasm_entropy_import_writes_injected_nonzero_bytes() {
        let state = RuntimeState {
            fixed_entropy: Some(0xa5),
            ..RuntimeState::default()
        };
        let mut runtime = OriginalWaterRuntime::new_with_state(state).unwrap();
        runtime.tick().unwrap();

        let writes = &runtime.store.data().entropy_writes;
        assert!(!writes.is_empty());
        assert!(
            writes
                .iter()
                .all(|(_, bytes)| !bytes.is_empty() && bytes.iter().all(|byte| *byte == 0xa5))
        );
    }
}
