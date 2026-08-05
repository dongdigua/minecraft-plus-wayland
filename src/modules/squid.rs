use std::error::Error;

use rand::Rng;

use super::{FrameInfo, Module, RenderContext, web_surface_fragment_entry};

const SQUID_ATLAS_WIDTH: u32 = 128;
const SQUID_ATLAS_HEIGHT: u32 = 640;
const SQUID_INSTANCE_COUNT: u32 = 32;

/// Native wgpu implementation of Web module=8.
///
/// The size, run duration, position, animation cycle, and atlas variant are
/// deliberately calculated in the vertex shader from one stable module seed
/// and `instance_index`, matching the original WebGL module's model.
pub struct SquidModule {
    pipeline: Option<wgpu::RenderPipeline>,
    bind_group: Option<wgpu::BindGroup>,
    uniforms: Option<wgpu::Buffer>,
    seed: f32,
}

impl Default for SquidModule {
    fn default() -> Self {
        Self {
            pipeline: None,
            bind_group: None,
            uniforms: None,
            seed: random_seed(),
        }
    }
}

impl Module for SquidModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let atlas = crate::resources::load_rgba_png("squids.png")?;
        if atlas.dimensions() != (SQUID_ATLAS_WIDTH, SQUID_ATLAS_HEIGHT) {
            return Err(format!(
                "squids.png must be {SQUID_ATLAS_WIDTH}x{SQUID_ATLAS_HEIGHT}, got {}x{}",
                atlas.width(),
                atlas.height()
            )
            .into());
        }

        let uniform_layout =
            context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("squid bind group layout"),
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
                    label: Some("squid pipeline layout"),
                    bind_group_layouts: &[Some(&uniform_layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("squid shader"),
                source: wgpu::ShaderSource::Wgsl(SQUID_SHADER.into()),
            });

        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("squid atlas"),
            size: wgpu::Extent3d {
                width: SQUID_ATLAS_WIDTH,
                height: SQUID_ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // WebGL samples the decoded PNG bytes as numeric RGBA values.
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
            atlas.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SQUID_ATLAS_WIDTH * 4),
                rows_per_image: Some(SQUID_ATLAS_HEIGHT),
            },
            texture.size(),
        );
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("squid atlas sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("squid uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.bind_group = Some(
            context
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("squid bind group"),
                    layout: &uniform_layout,
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
                label: Some("squid pipeline"),
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
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            },
        ));
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
            .expect("SquidModule was not initialized");
        let bind_group = self
            .bind_group
            .as_ref()
            .expect("SquidModule was not initialized");
        let uniforms = self
            .uniforms
            .as_ref()
            .expect("SquidModule was not initialized");
        context.queue.write_buffer(
            uniforms,
            0,
            &uniform_bytes(
                frame.elapsed.as_secs_f32(),
                frame.size.width.max(1) as f32,
                frame.size.height.max(1) as f32,
                self.seed,
            ),
        );

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("squid module"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    }),
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
        pass.draw(0..6, 0..SQUID_INSTANCE_COUNT);
    }
}

/// Match the Web module's `thread_rng().gen::<f32>()`: rand 0.6 uses the
/// HC-128-backed thread RNG and maps the upper 24 bits to `[0, 1)`.
fn random_seed() -> f32 {
    rand::thread_rng().r#gen()
}

fn uniform_bytes(time: f32, width: f32, height: f32, seed: f32) -> [u8; 16] {
    let mut bytes = [0; 16];
    for (index, value) in [time, width, height, seed].into_iter().enumerate() {
        bytes[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

const SQUID_SHADER: &str = r#"
struct ModuleUniforms {
    time: f32,
    width: f32,
    height: f32,
    seed: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: ModuleUniforms;
@group(0) @binding(1) var squid_atlas: texture_2d<f32>;
@group(0) @binding(2) var squid_sampler: sampler;

fn rand(value: f32) -> f32 {
    return fract(sin(value * 12.9898 + uniforms.seed) * 43758.5453);
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    // Exact six vertices and vTex values captured from the original
    // module=8 VBO, drawn with GL_TRIANGLES / TriangleList.
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(-32.0, -32.0),
        vec2<f32>(32.0, -32.0),
        vec2<f32>(-32.0, 32.0),
        vec2<f32>(32.0, -32.0),
        vec2<f32>(32.0, 32.0),
        vec2<f32>(-32.0, 32.0),
    );
    let quad_uv = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );

    let fid = f32(instance_index);
    let scale = max(0.5, 2.5 * rand(17.0 * fid + 3.0 + 7.0));
    let run_duration = max(2.0 / scale, 1.0) * (5.0 + 30.0 * rand(7.0 * fid + 2.0));
    let time_offset = 3.0 + 5.0 * rand(23.0 * fid + 1.0);
    let run = (uniforms.time + time_offset) / run_duration;
    let run_integer = floor(run);
    let cycles_per_run = max(2.0 / scale, 1.0)
        * (5.0 + 6.0 * rand(3.0 * fid + 11.0 + 3.0 * run_integer));
    let delta_y = uniforms.height * (2.0 * rand(13.0 * fid + 5.0 * run_integer) - 1.0);
    let variant = floor(min(run_integer / 30.0, 1.0)
        * 1.02 * rand(7.0 * fid + 2.0 * run_integer));

    let cycle = fract(run) * cycles_per_run;
    let cycle_phase = fract(cycle);
    let frame = floor(cycle_phase * 10.0);
    let progress = (floor(cycle) + pow(cycle_phase, 8.0)) / cycles_per_run;
    let sprite_size = 64.0 * scale;
    let x = (1.0 - progress) * (uniforms.width + sprite_size) - sprite_size;
    let y = (1.0 - progress) * uniforms.height + delta_y;
    // The original module=8 shader applies scale and translation only. In
    // particular, it does not rotate or swap vPosition's axes.
    let pixel_position = positions[vertex_index] * scale + vec2<f32>(x, y);

    var output: VertexOutput;
    output.position = vec4<f32>(
        pixel_position.x * 2.0 / uniforms.width - 1.0,
        pixel_position.y * 2.0 / uniforms.height - 1.0,
        // WebGPU clips negative Z whereas WebGL accepts the original
        // `-scale / 4.0`; no depth buffer is used, so preserve visibility.
        0.0,
        1.0,
    );
    output.uv = vec2<f32>((quad_uv[vertex_index].x + variant) / 2.0,
                          (1.0 - quad_uv[vertex_index].y + frame) / 10.0);
    return output;
}

fn srgb_to_linear(channel: f32) -> f32 {
    if (channel <= 0.04045) {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_srgb(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(squid_atlas, squid_sampler, input.uv);
    if (color.a == 0.0) {
        discard;
    }
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
    let color = textureSample(squid_atlas, squid_sampler, input.uv);
    if (color.a == 0.0) {
        discard;
    }
    return color;
}
"#;

#[cfg(test)]
mod tests {
    use super::uniform_bytes;

    #[test]
    fn uniforms_use_native_f32_layout() {
        let bytes = uniform_bytes(1.5, 1920.0, 1080.0, 0.25);
        assert_eq!(&bytes[0..4], &1.5f32.to_ne_bytes());
        assert_eq!(&bytes[4..8], &1920.0f32.to_ne_bytes());
        assert_eq!(&bytes[8..12], &1080.0f32.to_ne_bytes());
        assert_eq!(&bytes[12..16], &0.25f32.to_ne_bytes());
    }
}
