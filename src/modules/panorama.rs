use std::error::Error;

use rand::{Rng, seq::SliceRandom};

use super::{FrameInfo, Module, RenderContext, RenderSize, web_surface_fragment_entry};

const PANORAMA_LIST_RESOURCE: &str = "panoramas.txt";
const PANORAMA_PREFIX: &str = "panoramas";
const CUBEMAP_FACE_COUNT: u32 = 6;
const CUBEMAP_FACE_SIZE: u32 = 1024;
const CUBEMAP_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
// The source bundle uses four horizontal faces followed by top and bottom;
// WebGL uploads them to cubemap layers as +X <- 1, -X <- 3, +Y <- 4,
// -Y <- 5, +Z <- 0, -Z <- 2.
const SOURCE_FACE_FOR_CUBEMAP_LAYER: [u32; CUBEMAP_FACE_COUNT as usize] = [1, 3, 4, 5, 0, 2];
const DEGREES_TO_RADIANS: f32 = std::f32::consts::PI / 180.0;

/// Native wgpu implementation of Web module=6 (`panorama`).
///
/// A single panorama is selected at initialization, uploaded in the original
/// `panorama_0` through `panorama_5` cubemap-layer order, and retained for the
/// module lifetime. The camera follows the verified Web curve: a random
/// initial yaw plus one degree per second, with a ±20 degree pitch wobble.
pub struct PanoramaModule {
    pipeline: Option<wgpu::RenderPipeline>,
    bind_group: Option<wgpu::BindGroup>,
    uniforms: Option<wgpu::Buffer>,
    initial_yaw_degrees: f32,
}

impl Default for PanoramaModule {
    fn default() -> Self {
        Self {
            pipeline: None,
            bind_group: None,
            uniforms: None,
            initial_yaw_degrees: 0.0,
        }
    }
}

impl Module for PanoramaModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let panorama_names = panorama_names()?;
        let mut random = rand::thread_rng();
        let panorama_name = panorama_names
            .choose(&mut random)
            .ok_or("cannot choose from an empty panorama list")?;
        self.initial_yaw_degrees = random.gen_range(0.0_f32, 360.0);

        let faces = load_faces(panorama_name)?;
        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("panorama cubemap"),
            size: wgpu::Extent3d {
                width: CUBEMAP_FACE_SIZE,
                height: CUBEMAP_FACE_SIZE,
                depth_or_array_layers: CUBEMAP_FACE_COUNT,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // WebGL filters the decoded PNG channel values without an sRGB
            // decode, so keep interpolation in that same numeric domain.
            format: CUBEMAP_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (layer, face) in faces.iter().enumerate() {
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
                face.as_raw(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(CUBEMAP_FACE_SIZE * 4),
                    rows_per_image: Some(CUBEMAP_FACE_SIZE),
                },
                wgpu::Extent3d {
                    width: CUBEMAP_FACE_SIZE,
                    height: CUBEMAP_FACE_SIZE,
                    depth_or_array_layers: 1,
                },
            );
        }
        // WebGPU cubemap array layers use +X, -X, +Y, -Y, +Z, -Z. `load_faces`
        // has already placed the bundle's 0..5 files in the captured WebGL
        // target order for those layers.
        let cubemap_view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("panorama cubemap view"),
            dimension: Some(wgpu::TextureViewDimension::Cube),
            array_layer_count: Some(CUBEMAP_FACE_COUNT),
            ..Default::default()
        });
        let sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("panorama cubemap sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let bind_group_layout =
            context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("panorama bind group layout"),
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
                                view_dimension: wgpu::TextureViewDimension::Cube,
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
                    label: Some("panorama pipeline layout"),
                    bind_group_layouts: &[Some(&bind_group_layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("panorama shader"),
                source: wgpu::ShaderSource::Wgsl(PANORAMA_SHADER.into()),
            });
        let uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("panorama view uniforms"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.bind_group = Some(
            context
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("panorama bind group"),
                    layout: &bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniforms.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&cubemap_view),
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
                label: Some("panorama pipeline"),
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
                    front_face: wgpu::FrontFace::Ccw,
                    // The six fullscreen vertices are all front-facing, but
                    // no culling avoids depending on WebGL/WebGPU Y-axis
                    // conventions for this screen-space primitive.
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
            .expect("PanoramaModule was not initialized");
        let bind_group = self
            .bind_group
            .as_ref()
            .expect("PanoramaModule was not initialized");
        let uniforms = self
            .uniforms
            .as_ref()
            .expect("PanoramaModule was not initialized");
        context.queue.write_buffer(
            uniforms,
            0,
            &view_matrix_bytes(
                self.initial_yaw_degrees,
                frame.elapsed.as_secs_f32(),
                frame.size,
            ),
        );

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("panorama module"),
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

fn panorama_names() -> Result<Vec<String>, Box<dyn Error>> {
    let names = crate::resources::load_utf8(PANORAMA_LIST_RESOURCE)?;
    let names = names
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if names.is_empty() {
        return Err("panoramas.txt contains no panorama names".into());
    }
    if names
        .iter()
        .any(|name| name.contains('/') || name.contains('\\'))
    {
        return Err("panoramas.txt contains an invalid panorama name".into());
    }
    Ok(names)
}

fn load_faces(
    panorama_name: &str,
) -> Result<[image::RgbaImage; CUBEMAP_FACE_COUNT as usize], Box<dyn Error>> {
    let mut faces = Vec::with_capacity(CUBEMAP_FACE_COUNT as usize);
    for source_face in SOURCE_FACE_FOR_CUBEMAP_LAYER {
        let resource = format!("{PANORAMA_PREFIX}/{panorama_name}/panorama_{source_face}.png");
        let image = crate::resources::load_rgba_png(&resource)?;
        if image.dimensions() != (CUBEMAP_FACE_SIZE, CUBEMAP_FACE_SIZE) {
            return Err(format!(
                "{resource} must be {CUBEMAP_FACE_SIZE}x{CUBEMAP_FACE_SIZE}, got {}x{}",
                image.width(),
                image.height()
            )
            .into());
        }
        // The WebGL module gives the decoder's pixels directly to texImage2D;
        // preserve that pixel order. Only the image-to-cubemap-layer mapping
        // above differs from the ZIP file numbering.
        faces.push(image);
    }
    faces
        .try_into()
        .map_err(|_| "panorama face count does not match the cubemap layout".into())
}

/// Produces the original column-major `ScaleX(aspect) * RotateY(yaw) * RotateX(pitch)` matrix.
fn view_matrix(initial_yaw_degrees: f32, elapsed_seconds: f32, size: RenderSize) -> [[f32; 4]; 4] {
    let elapsed_seconds = elapsed_seconds.max(0.0);
    let yaw = (initial_yaw_degrees + elapsed_seconds) * DEGREES_TO_RADIANS;
    let pitch =
        (elapsed_seconds / 13.0).sin() * (elapsed_seconds / 5.0).sin() * 20.0 * DEGREES_TO_RADIANS;
    let aspect = size.width.max(1) as f32 / size.height.max(1) as f32;
    let (sine_yaw, cosine_yaw) = yaw.sin_cos();
    let (sine_pitch, cosine_pitch) = pitch.sin_cos();

    [
        [aspect * cosine_yaw, 0.0, -sine_yaw, 0.0],
        [
            aspect * sine_yaw * sine_pitch,
            cosine_pitch,
            cosine_yaw * sine_pitch,
            0.0,
        ],
        [
            aspect * sine_yaw * cosine_pitch,
            -sine_pitch,
            cosine_yaw * cosine_pitch,
            0.0,
        ],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

fn view_matrix_bytes(initial_yaw_degrees: f32, elapsed_seconds: f32, size: RenderSize) -> [u8; 64] {
    let matrix = view_matrix(initial_yaw_degrees, elapsed_seconds, size);
    let mut bytes = [0; 64];
    for (index, value) in matrix.into_iter().flatten().enumerate() {
        bytes[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

const PANORAMA_SHADER: &str = r#"
struct CameraUniforms {
    view: mat4x4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) beam: vec3<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: CameraUniforms;
@group(0) @binding(1) var panorama: texture_cube<f32>;
@group(0) @binding(2) var panorama_sampler: sampler;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
    );
    let position = positions[vertex_index];
    var output: VertexOutput;
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.beam = (uniforms.view * vec4<f32>(position, 1.0, 1.0)).xyz;
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
    let webgl_color = textureSample(panorama, panorama_sampler, input.beam);
    return vec4<f32>(
        srgb_to_linear(webgl_color.r),
        srgb_to_linear(webgl_color.g),
        srgb_to_linear(webgl_color.b),
        1.0,
    );
}

@fragment
fn fs_unorm(input: VertexOutput) -> @location(0) vec4<f32> {
    let webgl_color = textureSample(panorama, panorama_sampler, input.beam);
    return vec4<f32>(webgl_color.rgb, 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::{CUBEMAP_FORMAT, RenderSize, view_matrix, view_matrix_bytes};

    #[test]
    fn view_matrix_matches_the_captured_web_animation() {
        let matrix = view_matrix(
            26.315,
            0.1,
            RenderSize {
                width: 1280,
                height: 720,
            },
        );
        assert!((matrix[0][0] - 1.592_161_8).abs() < 0.000_01);
        assert!((matrix[0][2] - -0.444_878_4).abs() < 0.000_01);
        assert!((matrix[1][0] - 0.000_042_47).abs() < 0.000_001);
        assert!((matrix[2][1] - -0.000_053_70).abs() < 0.000_001);
    }

    #[test]
    fn uniforms_are_a_tightly_packed_column_major_mat4() {
        let bytes = view_matrix_bytes(
            0.0,
            0.0,
            RenderSize {
                width: 1,
                height: 1,
            },
        );
        assert_eq!(&bytes[0..4], &1.0f32.to_ne_bytes());
        assert_eq!(&bytes[60..64], &1.0f32.to_ne_bytes());
    }

    #[test]
    fn cubemap_filters_in_the_web_numeric_domain() {
        assert_eq!(CUBEMAP_FORMAT, wgpu::TextureFormat::Rgba8Unorm);
    }
}
