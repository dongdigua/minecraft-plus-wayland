use std::error::Error;

use rand::{Rng, RngCore, SeedableRng};
use rand_hc::Hc128Rng;

use super::{FrameInfo, Module, RenderContext, RenderSize, web_surface_fragment_entry};

const GRASS_RESOURCE: &str = "grass/grass.png";
const DIRT_RESOURCE: &str = "grass/dirt.png";
const BIOME_COLORS_RESOURCE: &str = "grass/colors.png";
const TILE_PIXELS: u32 = 120;
const UPDATE_PERIOD_SECONDS: f64 = 0.025;
const UNIFORM_BYTES: u64 = 32;

/// Native wgpu implementation of Web module=10 (`grass`).
///
/// The Web module stores a biome pair per tile.  A pair whose components sum
/// to at most one is grass; `(1, 1)` is dirt.  At most one random, four-way
/// diffusion candidate is evaluated for each 25 ms time bucket.
pub struct GrassModule {
    pipeline: Option<wgpu::RenderPipeline>,
    bind_group_layout: Option<wgpu::BindGroupLayout>,
    bind_group: Option<wgpu::BindGroup>,
    uniforms: Option<wgpu::Buffer>,
    biome_buffer: Option<wgpu::Buffer>,
    grass_view: Option<wgpu::TextureView>,
    dirt_view: Option<wgpu::TextureView>,
    colors_view: Option<wgpu::TextureView>,
    sampler: Option<wgpu::Sampler>,
    state: GrassState,
    seed: f32,
}

impl Default for GrassModule {
    fn default() -> Self {
        // This grid is replaced by the first configured output size.
        let state = GrassState::new(20, 1, 1, random_seed());
        Self {
            pipeline: None,
            bind_group_layout: None,
            bind_group: None,
            uniforms: None,
            biome_buffer: None,
            grass_view: None,
            dirt_view: None,
            colors_view: None,
            sampler: None,
            seed: state.shader_seed,
            state,
        }
    }
}

impl Module for GrassModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let grass = load_texture(context, GRASS_RESOURCE, (16, 16))?;
        let dirt = load_texture(context, DIRT_RESOURCE, (16, 16))?;
        let colors = load_texture(context, BIOME_COLORS_RESOURCE, (256, 256))?;
        let sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("grass nearest sampler"),
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
                label: Some("grass bind group layout"),
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
                    texture_layout_entry(2),
                    texture_layout_entry(3),
                    texture_layout_entry(4),
                    wgpu::BindGroupLayoutEntry {
                        binding: 5,
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
                    label: Some("grass pipeline layout"),
                    bind_group_layouts: &[Some(&layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("grass shader"),
                source: wgpu::ShaderSource::Wgsl(GRASS_SHADER.into()),
            });
        self.pipeline = Some(context.device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("grass pipeline"),
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
                    // The captured Web VBO is CCW after its VP transform.
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: Some(wgpu::Face::Back),
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                // The Web module explicitly disables GL_DEPTH_TEST.
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            },
        ));
        self.bind_group_layout = Some(layout);
        self.grass_view = Some(grass);
        self.dirt_view = Some(dirt);
        self.colors_view = Some(colors);
        self.sampler = Some(sampler);
        self.recreate_grid_resources(context);
        Ok(())
    }

    fn resize(&mut self, context: &RenderContext<'_>, size: RenderSize) {
        let dimensions = grid_dimensions(size);
        if self.state.allocated_columns == dimensions.allocated_columns
            && self.state.visible_columns == dimensions.visible_columns
            && self.state.rows == dimensions.rows
        {
            return;
        }
        self.state = GrassState::new(
            dimensions.allocated_columns,
            dimensions.visible_columns,
            dimensions.rows,
            random_seed(),
        );
        self.seed = self.state.shader_seed;
        self.recreate_grid_resources(context);
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
            .expect("GrassModule was not initialized");
        let bind_group = self
            .bind_group
            .as_ref()
            .expect("GrassModule was not initialized");
        let uniforms = self
            .uniforms
            .as_ref()
            .expect("GrassModule was not initialized");
        let biome_buffer = self
            .biome_buffer
            .as_ref()
            .expect("GrassModule was not initialized");

        context.queue.write_buffer(
            uniforms,
            0,
            &uniform_bytes(
                self.state.allocated_columns,
                self.state.visible_columns,
                self.state.rows,
                self.seed,
                self.state.offset,
                frame.elapsed.as_secs_f32(),
                self.state.shift_start,
            ),
        );
        if self.state.dirty {
            context
                .queue
                .write_buffer(biome_buffer, 0, &biome_bytes(&self.state.biomes));
            self.state.dirty = false;
        }

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("grass module"),
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
        pass.draw(0..6, 0..self.state.biomes.len() as u32);
    }
}

impl GrassModule {
    fn recreate_grid_resources(&mut self, context: &RenderContext<'_>) {
        let buffer = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("grass biome state"),
            size: (self.state.biomes.len().max(1) * 8) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        context
            .queue
            .write_buffer(&buffer, 0, &biome_bytes(&self.state.biomes));
        self.state.dirty = false;

        let layout = self
            .bind_group_layout
            .as_ref()
            .expect("grass layout must exist");
        let uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("grass uniforms"),
            size: UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.bind_group = Some(
            context
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("grass bind group"),
                    layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniforms.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(
                                self.grass_view.as_ref().expect("grass texture must exist"),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(
                                self.dirt_view.as_ref().expect("dirt texture must exist"),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::TextureView(
                                self.colors_view.as_ref().expect("biome texture must exist"),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: wgpu::BindingResource::Sampler(
                                self.sampler.as_ref().expect("grass sampler must exist"),
                            ),
                        },
                    ],
                }),
        );
        self.uniforms = Some(uniforms);
        self.biome_buffer = Some(buffer);
    }
}

struct GrassState {
    allocated_columns: u32,
    visible_columns: u32,
    rows: u32,
    biomes: Vec<[f32; 2]>,
    last_bucket: Option<u64>,
    last_shift: f64,
    offset: u32,
    shift_start: f32,
    shader_seed: f32,
    random: Hc128Rng,
    dirty: bool,
}

impl GrassState {
    fn new(allocated_columns: u32, visible_columns: u32, rows: u32, seed: [u8; 32]) -> Self {
        let mut random = Hc128Rng::from_seed(seed);
        let mut biomes = vec![[1.0, 1.0]; (allocated_columns * rows) as usize];
        // The Web module begins with its left-hand working column as grass.
        for row in 0..rows {
            biomes[row as usize] = [0.0, 0.0];
        }
        Self {
            allocated_columns,
            visible_columns,
            rows,
            biomes,
            last_bucket: None,
            last_shift: -1.0,
            offset: 0,
            shift_start: -1.0,
            // HC-128 is also the Web build's RNG implementation. The seed
            // material is native-local, so launches need not match the Web
            // crypto-seeded sequence item-for-item.
            shader_seed: random.gen_range(0.0, 1.0),
            random,
            dirty: true,
        }
    }

    fn advance(&mut self, elapsed_seconds: f64) {
        let elapsed_seconds = elapsed_seconds.max(0.0);
        let bucket = (elapsed_seconds / UPDATE_PERIOD_SECONDS).floor() as u64;
        if self.last_bucket == Some(bucket) {
            return;
        }
        // Match the Web callback: a delayed frame evaluates its current bucket
        // once, rather than looping through all missed 25 ms buckets.
        self.last_bucket = Some(bucket);

        let (first_column, column_end) = candidate_column_bounds(self.allocated_columns);
        let column = self.random.gen_range(first_column, column_end);
        let row = self.random.gen_range(0, self.rows);
        // In the Web state machine, candidates in the work area (column > 9)
        // trigger a one-column recycle/scroll, but never more than once per second.
        let moves_view = column > 9;
        if moves_view && elapsed_seconds - self.last_shift <= 1.0 {
            return;
        }

        let index = self.index(column, row);
        let mut total = [0.0, 0.0];
        let mut count = 0.0;
        self.accumulate_neighbor(column - 1, row, &mut total, &mut count);
        if column + 1 < self.allocated_columns {
            self.accumulate_neighbor(column + 1, row, &mut total, &mut count);
        }
        if row > 0 {
            self.accumulate_neighbor(column, row - 1, &mut total, &mut count);
        }
        if row + 1 < self.rows {
            self.accumulate_neighbor(column, row + 1, &mut total, &mut count);
        }
        if count == 0.0 || self.random.gen_range(0, 4) == 3 {
            return;
        }

        let target = match self.random.gen_range(0, 3) {
            0 => [0.0, 0.0],
            1 => [1.0, 0.0],
            _ => [0.0, 1.0],
        };
        let alpha = self.random.gen_range(0.0, 0.2);
        let average = [total[0] / count, total[1] / count];
        self.biomes[index] = [
            average[0] + alpha * (target[0] - average[0]),
            average[1] + alpha * (target[1] - average[1]),
        ];
        self.dirty = true;

        if moves_view {
            self.scroll_left(elapsed_seconds);
        }
    }

    fn accumulate_neighbor(&self, column: u32, row: u32, total: &mut [f32; 2], count: &mut f32) {
        let neighbor = self.biomes[self.index(column, row)];
        if is_grass(neighbor) {
            total[0] += neighbor[0];
            total[1] += neighbor[1];
            *count += 1.0;
        }
    }

    fn scroll_left(&mut self, elapsed_seconds: f64) {
        let column_len = self.rows as usize;
        let final_column_start = self.biomes.len() - column_len;
        self.biomes.copy_within(column_len.., 0);
        self.biomes[final_column_start..].fill([1.0, 1.0]);
        self.offset = self.offset.wrapping_add(1);
        self.last_shift = elapsed_seconds;
        self.shift_start = elapsed_seconds as f32;
        self.dirty = true;
    }

    fn index(&self, column: u32, row: u32) -> usize {
        (column * self.rows + row) as usize
    }
}

fn is_grass(biome: [f32; 2]) -> bool {
    biome[0] + biome[1] <= 1.0
}

fn load_texture(
    context: &RenderContext<'_>,
    resource: &str,
    expected_dimensions: (u32, u32),
) -> Result<wgpu::TextureView, Box<dyn Error>> {
    let image = crate::resources::load_rgba_png(resource)?;
    if image.dimensions() != expected_dimensions {
        return Err(format!(
            "{resource} must be {}x{}, got {}x{}",
            expected_dimensions.0,
            expected_dimensions.1,
            image.width(),
            image.height()
        )
        .into());
    }
    let texture = context.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(resource),
        size: wgpu::Extent3d {
            width: image.width(),
            height: image.height(),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        // WebGL samples the PNG decoder's numeric texels, without sRGB decode.
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
            bytes_per_row: Some(image.width() * 4),
            rows_per_image: Some(image.height()),
        },
        texture.size(),
    );
    Ok(texture.create_view(&wgpu::TextureViewDescriptor::default()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GridDimensions {
    allocated_columns: u32,
    visible_columns: u32,
    rows: u32,
}

fn grid_dimensions(size: RenderSize) -> GridDimensions {
    let rows = size.height.max(1).div_ceil(TILE_PIXELS).max(1);
    let visible_columns = size.width.max(1).div_ceil(TILE_PIXELS).max(1);
    let allocated_columns = 20.max(2 * visible_columns);
    GridDimensions {
        allocated_columns,
        visible_columns,
        rows,
    }
}

fn candidate_column_bounds(allocated_columns: u32) -> (u32, u32) {
    (1, allocated_columns)
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

fn texture_layout_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
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

fn uniform_bytes(
    allocated_columns: u32,
    visible_columns: u32,
    rows: u32,
    seed: f32,
    offset: u32,
    time: f32,
    shift_start: f32,
) -> [u8; UNIFORM_BYTES as usize] {
    // Eight scalar slots keep the WGSL uniform struct at 32 bytes while
    // carrying both the storage width and the independent screen VP width.
    let values = [
        allocated_columns as f32,
        visible_columns as f32,
        rows as f32,
        seed,
        offset as f32,
        time,
        shift_start,
        0.0,
    ];
    let mut bytes = [0; UNIFORM_BYTES as usize];
    for (index, value) in values.into_iter().enumerate() {
        bytes[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

fn biome_bytes(biomes: &[[f32; 2]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(biomes.len() * 8);
    for biome in biomes {
        bytes.extend_from_slice(&biome[0].to_ne_bytes());
        bytes.extend_from_slice(&biome[1].to_ne_bytes());
    }
    bytes
}

fn random_seed() -> [u8; 32] {
    let mut seed = [0; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    seed
}

const GRASS_SHADER: &str = r#"
struct ModuleUniforms {
    allocated_columns: f32,
    visible_columns: f32,
    rows: f32,
    seed: f32,
    offset: f32,
    time: f32,
    shift_start: f32,
    _padding: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) biome: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: ModuleUniforms;
@group(0) @binding(1) var<storage, read> biomes: array<vec2<f32>>;
@group(0) @binding(2) var grass_texture: texture_2d<f32>;
@group(0) @binding(3) var dirt_texture: texture_2d<f32>;
@group(0) @binding(4) var biome_colors: texture_2d<f32>;
@group(0) @binding(5) var nearest_sampler: sampler;

fn rand(value: f32) -> f32 {
    return fract(sin(value * 12.9898 + uniforms.seed) * 43758.5453);
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    // Exact module=10 WebGL VBO order: two CCW triangles, position.xy then UV.
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let texcoords = positions;
    let column_height = u32(uniforms.rows);
    let column = instance_index / column_height;
    let row = instance_index - column * column_height;
    let rotation = 1.5707963267948966
        * floor(4.0 * rand(11.0 * (f32(column) + uniforms.offset)
                           + 13.0 * f32(row) + 3.0));
    let s = sin(rotation);
    let c = cos(rotation);
    let position = positions[vertex_index] - vec2<f32>(0.5, 0.5);
    // GLSL mat3 constructors are column-major. The Web shader's quarter-turn
    // matrix therefore maps (x, y) to (c*x + s*y, -s*x + c*y).
    let rotated = vec2<f32>(c * position.x + s * position.y,
                            -s * position.x + c * position.y) + vec2<f32>(0.5, 0.5);

    var output: VertexOutput;
    // The captured Web state starts at shift_start = -1. Each recycled
    // column resets shift_start, then slides left for one second.
    let shift = min(uniforms.time - uniforms.shift_start, 1.0);
    let grid_position = vec2<f32>(f32(column) - shift + rotated.x,
                                  f32(row) + rotated.y);
    output.position = vec4<f32>(
        grid_position.x * 2.0 / uniforms.visible_columns - 1.0,
        grid_position.y * 2.0 / uniforms.rows - 1.0,
        0.0,
        1.0,
    );
    // WebGL's raw texImage2D upload treats PNG row zero as texture bottom;
    // wgpu samples row zero at the top, so invert V at the native boundary.
    output.uv = vec2<f32>(texcoords[vertex_index].x, 1.0 - texcoords[vertex_index].y);
    output.biome = biomes[instance_index];
    return output;
}

fn srgb_to_linear(channel: f32) -> f32 {
    if (channel <= 0.04045) {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

fn web_color(input: VertexOutput) -> vec4<f32> {
    var tile: vec4<f32>;
    if (input.biome.x + input.biome.y <= 1.0) {
        tile = textureSample(grass_texture, nearest_sampler, input.uv);
    } else {
        tile = textureSample(dirt_texture, nearest_sampler, input.uv);
    }
    // The source's biome pairs use the PNG's native lookup orientation:
    // `(1, 1)` must hit its neutral white corner so dirt keeps its brown RGB.
    let biome = textureSample(biome_colors, nearest_sampler, input.biome);
    return vec4<f32>(tile.rgb * biome.rgb, 1.0);
}

@fragment
fn fs_srgb(input: VertexOutput) -> @location(0) vec4<f32> {
    // Compute the complete Web numeric result before compensating for the
    // surface target's automatic sRGB encode.
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
    use super::{
        GrassState, GridDimensions, RenderSize, UNIFORM_BYTES, UPDATE_PERIOD_SECONDS,
        candidate_column_bounds, grid_dimensions, is_grass, uniform_bytes,
    };

    fn glsl_column_major_rotation(position: [f32; 2], rotation: f32) -> [f32; 2] {
        let (s, c) = rotation.sin_cos();
        [
            c * position[0] + s * position[1],
            -s * position[0] + c * position[1],
        ]
    }

    #[test]
    fn pi_over_two_rotation_matches_glsl_column_major_mat3() {
        let rotated = glsl_column_major_rotation([0.25, -0.5], std::f32::consts::FRAC_PI_2);
        assert!((rotated[0] + 0.5).abs() < 1.0e-6);
        assert!((rotated[1] + 0.25).abs() < 1.0e-6);
    }

    #[test]
    fn grid_dimensions_match_web_allocation_strategy() {
        for (width, visible_columns, allocated_columns) in
            [(800, 7, 20), (960, 8, 20), (1280, 11, 22), (1920, 16, 32)]
        {
            assert_eq!(
                grid_dimensions(RenderSize {
                    width,
                    height: 1080,
                }),
                GridDimensions {
                    allocated_columns,
                    visible_columns,
                    rows: 9,
                }
            );
        }
    }

    #[test]
    fn candidate_range_reaches_the_allocated_work_area() {
        for allocated_columns in [20, 22, 32] {
            let (start, end) = candidate_column_bounds(allocated_columns);
            assert_eq!(start, 1);
            assert_eq!(end, allocated_columns);
            assert!(start <= 10 && 10 < end, "column > 9 must be reachable");
            assert_eq!(end - 1, allocated_columns - 1);
        }
    }

    #[test]
    fn state_storage_uses_allocated_columns() {
        let state = GrassState::new(22, 11, 9, [3; 32]);
        assert_eq!(state.biomes.len(), 22 * 9);
        assert_eq!(state.visible_columns, 11);
    }

    #[test]
    fn uniforms_keep_the_32_byte_wgsl_scalar_layout() {
        let bytes = uniform_bytes(32, 16, 9, 0.25, 3, 4.5, 2.5);
        assert_eq!(bytes.len(), UNIFORM_BYTES as usize);
        let values = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_ne_bytes(chunk.try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(values, [32.0, 16.0, 9.0, 0.25, 3.0, 4.5, 2.5, 0.0]);
    }

    #[test]
    fn grass_predicate_matches_the_web_fragment_branch() {
        assert!(is_grass([0.6, 0.4]));
        assert!(!is_grass([0.6, 0.400_001]));
    }

    #[test]
    fn delayed_time_does_not_catch_up_multiple_buckets() {
        let mut state = GrassState::new(18, 16, 9, [1; 32]);
        state.advance(0.0);
        let after_first = state.last_bucket;
        state.advance(UPDATE_PERIOD_SECONDS * 100.0);
        assert_eq!(state.last_bucket, Some(100));
        assert_ne!(after_first, state.last_bucket);
    }

    #[test]
    fn recycling_a_column_preserves_the_world_rotation_coordinate() {
        let mut state = GrassState::new(4, 2, 2, [2; 32]);
        state.biomes = vec![
            [0.0, 0.0],
            [0.1, 0.0], // column 0
            [0.2, 0.0],
            [0.3, 0.0], // column 1
            [0.4, 0.0],
            [0.5, 0.0], // column 2
            [0.6, 0.0],
            [0.7, 0.0], // column 3
        ];
        state.scroll_left(12.5);
        assert_eq!(
            state.biomes[0..6],
            [
                [0.2, 0.0],
                [0.3, 0.0],
                [0.4, 0.0],
                [0.5, 0.0],
                [0.6, 0.0],
                [0.7, 0.0]
            ]
        );
        assert_eq!(state.biomes[6..], [[1.0, 1.0], [1.0, 1.0]]);
        assert_eq!(state.offset, 1);
        assert_eq!(state.shift_start, 12.5);
    }
}
