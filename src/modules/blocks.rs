use std::error::Error;

use rand::Rng;

use super::{FrameInfo, Module, RenderContext};

const BLOCK_LIST_RESOURCE: &str = "full_blocks.txt";
const BLOCK_TEXTURE_RESOURCE: &str = "full_blocks.png";
const ATLAS_DIMENSION: u32 = 512;
const TILE_DIMENSION: u32 = 16;
const UNIFORM_BYTES: u64 = 32;

/// Native wgpu implementation of Web module=11 (`blocks`).
///
/// The Web module renders one fullscreen quad. Its apparent camera is an
/// affine transform of an infinite, procedural texture-coordinate plane; it
/// is not a 3D camera. Both launch seeds remain fixed for this instance, just
/// as their WebGL uniforms do.
pub struct BlocksModule {
    pipeline: Option<wgpu::RenderPipeline>,
    bind_group: Option<wgpu::BindGroup>,
    uniforms: Option<wgpu::Buffer>,
    line_width: f32,
    slot_count: f32,
    vertex_seed: f32,
    fragment_seed: f32,
}

impl Default for BlocksModule {
    fn default() -> Self {
        let mut random = rand::thread_rng();
        Self {
            pipeline: None,
            bind_group: None,
            uniforms: None,
            line_width: 0.0,
            slot_count: 0.0,
            vertex_seed: random.gen_range(0.0f32, 1.0),
            fragment_seed: random.gen_range(0.0f32, 1.0),
        }
    }
}

impl Module for BlocksModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let (line_width, slot_count) = load_block_layout()?;
        let blocks = crate::resources::load_rgba_png(BLOCK_TEXTURE_RESOURCE)?;
        if blocks.dimensions() != (ATLAS_DIMENSION, ATLAS_DIMENSION) {
            return Err(format!(
                "{BLOCK_TEXTURE_RESOURCE} must be {ATLAS_DIMENSION}x{ATLAS_DIMENSION}, got {}x{}",
                blocks.width(),
                blocks.height()
            )
            .into());
        }
        if line_width * TILE_DIMENSION != ATLAS_DIMENSION {
            return Err(format!(
                "{BLOCK_LIST_RESOURCE} line width {line_width} does not describe a {ATLAS_DIMENSION}px, {TILE_DIMENSION}px-tile atlas"
            )
            .into());
        }
        let required_rows = slot_count.div_ceil(line_width);
        if required_rows * TILE_DIMENSION > ATLAS_DIMENSION {
            return Err(format!(
                "{BLOCK_LIST_RESOURCE} has {slot_count} slots, exceeding the {ATLAS_DIMENSION}x{ATLAS_DIMENSION} atlas"
            )
            .into());
        }

        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("module 11 blocks atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_DIMENSION,
                height: ATLAS_DIMENSION,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // The WebGL module uploads RGB bytes with no sRGB decode. `image`
            // expands the original RGB PNG to opaque RGBA; this format retains
            // the same numeric RGB samples for the WGSL hash/atlas lookup.
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
            blocks.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_DIMENSION * 4),
                rows_per_image: Some(ATLAS_DIMENSION),
            },
            texture.size(),
        );
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("module 11 blocks nearest sampler"),
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
                    label: Some("module 11 blocks bind group layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
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
                    label: Some("module 11 blocks pipeline layout"),
                    bind_group_layouts: &[Some(&bind_group_layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("module 11 blocks shader"),
                source: wgpu::ShaderSource::Wgsl(BLOCKS_SHADER.into()),
            });
        let uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("module 11 blocks uniforms"),
            size: UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("module 11 blocks bind group"),
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
            });

        self.pipeline = Some(context.device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("module 11 blocks pipeline"),
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
                    // The original VBO has two CCW triangles and enables
                    // CULL_FACE with its default back-face mode.
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: Some(wgpu::Face::Back),
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                // Web module=11 calls disable(GL_DEPTH_TEST).
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            },
        ));
        self.bind_group = Some(bind_group);
        self.uniforms = Some(uniforms);
        self.line_width = line_width as f32;
        self.slot_count = slot_count as f32;
        Ok(())
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
            .expect("BlocksModule was not initialized");
        let bind_group = self
            .bind_group
            .as_ref()
            .expect("BlocksModule was not initialized");
        let uniforms = self
            .uniforms
            .as_ref()
            .expect("BlocksModule was not initialized");

        context.queue.write_buffer(
            uniforms,
            0,
            &uniform_bytes(
                frame.size.width.max(1) as f32 / TILE_DIMENSION as f32,
                frame.size.height.max(1) as f32 / TILE_DIMENSION as f32,
                frame.elapsed.as_secs_f32(),
                self.vertex_seed,
                self.fragment_seed,
                self.line_width,
                self.slot_count,
            ),
        );

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("module 11 blocks"),
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
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..6, 0..1);
    }
}

fn load_block_layout() -> Result<(u32, u32), Box<dyn Error>> {
    let layout = crate::resources::load_utf8(BLOCK_LIST_RESOURCE)?;
    let mut lines = layout.lines();
    let line_width = parse_layout_value(lines.next(), "line width")?;
    let slot_count = parse_layout_value(lines.next(), "slot count")?;
    if line_width == 0 || slot_count == 0 {
        return Err(format!("{BLOCK_LIST_RESOURCE} must contain non-zero dimensions").into());
    }

    let listed_slots = lines.filter(|line| !line.trim().is_empty()).count();
    if listed_slots != slot_count as usize {
        return Err(format!(
            "{BLOCK_LIST_RESOURCE} declares {slot_count} slots but contains {listed_slots} entries"
        )
        .into());
    }
    Ok((line_width, slot_count))
}

fn parse_layout_value(value: Option<&str>, name: &str) -> Result<u32, Box<dyn Error>> {
    value
        .ok_or_else(|| format!("{BLOCK_LIST_RESOURCE} is missing {name}"))?
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("cannot parse {name} in {BLOCK_LIST_RESOURCE}: {error}").into())
}

fn uniform_bytes(
    slots_x: f32,
    slots_y: f32,
    time: f32,
    vertex_seed: f32,
    fragment_seed: f32,
    line_width: f32,
    slot_count: f32,
) -> [u8; UNIFORM_BYTES as usize] {
    // WGSL layout is two f32 values followed by six scalar f32 values.
    let values = [
        slots_x,
        slots_y,
        time,
        vertex_seed,
        fragment_seed,
        line_width,
        slot_count,
        0.0,
    ];
    let mut bytes = [0; UNIFORM_BYTES as usize];
    for (index, value) in values.into_iter().enumerate() {
        bytes[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

const BLOCKS_SHADER: &str = r#"
struct ModuleUniforms {
    slots_per_screen: vec2<f32>,
    time: f32,
    vertex_seed: f32,
    fragment_seed: f32,
    line_width: f32,
    slot_count: f32,
    _padding: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: ModuleUniforms;
@group(0) @binding(1) var blocks_texture: texture_2d<f32>;
@group(0) @binding(2) var blocks_sampler: sampler;

fn rotate_like_glsl_mat2(value: vec2<f32>, angle: f32) -> vec2<f32> {
    let s = sin(angle);
    let c = cos(angle);
    // GLSL's mat2(c, -s, s, c) constructor is column-major:
    // [ c  s ] * value, not the conventional row-major spelling.
    return vec2<f32>(c * value.x + s * value.y, -s * value.x + c * value.y);
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // Exact module=11 WebGL VBO order: position.xy, then vTex.xy.
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let texcoords = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 0.0),
    );
    let time = uniforms.time;
    let seed = uniforms.vertex_seed;
    let angle = 6.283185307179586 * sin(time / 35.0 + seed);
    let zoom_factor = -cos(time / 12.0 + seed * 2.0)
        * sin(time / 100.0 + seed * 3.0) + 1.7;
    let zoom = uniforms.slots_per_screen / 8.0 * zoom_factor;
    let translated_time = time / 100.0 + seed;
    let translation = vec2<f32>(
        50.0 * sin(11.0 * translated_time + seed)
            + 1000.0 * cos(13.0 * translated_time / 100.0 + 2.0 * seed)
                * sqrt(translated_time / 50.0),
        67.0 * cos(9.0 * translated_time + 3.0 * seed)
            + 1050.0 * sin(7.0 * translated_time / 100.0 + 4.0 * seed)
                * sqrt(translated_time / 50.0),
    );

    var output: VertexOutput;
    output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    output.tex = rotate_like_glsl_mat2(zoom * texcoords[vertex_index], angle) + translation;
    return output;
}

fn rand_like_glsl(value: f32) -> f32 {
    return fract(sin(value * 12.9898 + uniforms.fragment_seed) * 43758.5453);
}

fn srgb_to_linear(channel: f32) -> f32 {
    if (channel <= 0.04045) {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let slot_position = floor(input.tex);
    let slot_fraction = fract(input.tex);
    let slot_id = floor(uniforms.slot_count * rand_like_glsl(
        11.0 * slot_position.x + 23.0 * slot_position.y + 8.0,
    ));
    let slot_row = floor(slot_id / uniforms.line_width);
    let slot_column = slot_id - slot_row * uniforms.line_width;
    let angle = 1.5707963267948966 * floor(4.0 * rand_like_glsl(
        23.0 * slot_position.x + 19.0 * slot_position.y + 3.0,
    ));
    let position_in_slot = rotate_like_glsl_mat2(
        slot_fraction - vec2<f32>(0.5),
        angle,
    ) + vec2<f32>(0.5);
    let atlas_uv = (vec2<f32>(slot_column, slot_row) + position_in_slot)
        / uniforms.line_width;
    let webgl_color = textureSample(blocks_texture, blocks_sampler, atlas_uv);
    // The source texture is numeric RGB and the original WebGL default canvas
    // presents that numeric value as sRGB. Convert to linear for a Wayland
    // sRGB render target, as the other native image-backed modules do.
    return vec4<f32>(
        vec3<f32>(
            srgb_to_linear(webgl_color.r),
            srgb_to_linear(webgl_color.g),
            srgb_to_linear(webgl_color.b),
        ),
        1.0,
    );
}
"#;

#[cfg(test)]
mod tests {
    use super::{UNIFORM_BYTES, uniform_bytes};

    #[test]
    fn uniforms_follow_the_wgsl_scalar_layout() {
        let bytes = uniform_bytes(80.0, 45.0, 1.5, 0.25, 0.5, 32.0, 463.0);
        assert_eq!(bytes.len(), UNIFORM_BYTES as usize);
        assert_eq!(&bytes[0..4], &80.0f32.to_ne_bytes());
        assert_eq!(&bytes[4..8], &45.0f32.to_ne_bytes());
        assert_eq!(&bytes[8..12], &1.5f32.to_ne_bytes());
        assert_eq!(&bytes[12..16], &0.25f32.to_ne_bytes());
        assert_eq!(&bytes[16..20], &0.5f32.to_ne_bytes());
        assert_eq!(&bytes[20..24], &32.0f32.to_ne_bytes());
        assert_eq!(&bytes[24..28], &463.0f32.to_ne_bytes());
    }
}
