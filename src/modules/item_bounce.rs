use std::{collections::HashSet, error::Error, io::Cursor};

use obj::ObjData;
use rand::{Rng, RngCore, SeedableRng};
use rand_hc::Hc128Rng;

use super::{FrameInfo, Module, RenderContext, RenderSize, web_surface_fragment_entry};

const MANIFEST_RESOURCE: &str = "pop_items.txt";
const ATLAS_RESOURCE: &str = "item_models/atlas.png";
const MODEL_COUNT: usize = 1_044;
const ATLAS_DIMENSION: u32 = 1_024;
const INTERNAL_TICKS_PER_FRAME: f64 = 1.0 / 15.0;
const UPDATE_INTERVAL_SECONDS: f64 = 1.0 / 15.0;
const UNIFORM_BYTES: u64 = 96;
const TRAIL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Native wgpu implementation of Web `module=9`.
///
/// One random atlas-textured OBJ crosses the viewport while the vertex shader
/// performs the original Y rotation and exponentially damped absolute-cosine
/// bounce. Every accepted 15 Hz update is accumulated in an off-screen texture
/// which is only cleared when created, preserving trails across runs.
pub struct ItemBounceModule {
    item_pipeline: Option<wgpu::RenderPipeline>,
    copy_pipeline: Option<wgpu::RenderPipeline>,
    item_layout: Option<wgpu::BindGroupLayout>,
    copy_layout: Option<wgpu::BindGroupLayout>,
    atlas_view: Option<wgpu::TextureView>,
    atlas_sampler: Option<wgpu::Sampler>,
    _atlas: Option<wgpu::Texture>,
    uniforms: Option<wgpu::Buffer>,
    geometry: Option<Geometry>,
    trail: Option<TrailTarget>,
    copy_bind_group: Option<wgpu::BindGroup>,
    models: Vec<Model>,
    state: State,
    draw_this_frame: bool,
}

impl Default for ItemBounceModule {
    fn default() -> Self {
        Self {
            item_pipeline: None,
            copy_pipeline: None,
            item_layout: None,
            copy_layout: None,
            atlas_view: None,
            atlas_sampler: None,
            _atlas: None,
            uniforms: None,
            geometry: None,
            trail: None,
            copy_bind_group: None,
            models: Vec::new(),
            state: State::new(),
            draw_this_frame: false,
        }
    }
}

impl Module for ItemBounceModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        self.models = load_manifest()?;
        let image = crate::resources::load_rgba_png(ATLAS_RESOURCE)?;
        if image.dimensions() != (ATLAS_DIMENSION, ATLAS_DIMENSION) {
            return Err(format!(
                "{ATLAS_RESOURCE} must be {ATLAS_DIMENSION}x{ATLAS_DIMENSION}, got {}x{}",
                image.width(),
                image.height()
            )
            .into());
        }

        let atlas = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("item-bounce atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_DIMENSION,
                height: ATLAS_DIMENSION,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // WebGL samples the uploaded RGBA bytes numerically, without sRGB decode.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        context.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            image.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_DIMENSION * 4),
                rows_per_image: Some(ATLAS_DIMENSION),
            },
            atlas.size(),
        );
        let atlas_view = atlas.create_view(&Default::default());
        let atlas_sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("item-bounce nearest sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let item_layout =
            context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("item-bounce item layout"),
                    entries: &[uniform_binding(0), texture_binding(1), sampler_binding(2)],
                });
        let copy_layout =
            context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("item-bounce copy layout"),
                    entries: &[texture_binding(3), sampler_binding(4)],
                });
        let item_pipeline_layout =
            context
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("item-bounce item pipeline layout"),
                    bind_group_layouts: &[Some(&item_layout)],
                    immediate_size: 0,
                });
        let copy_pipeline_layout =
            context
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("item-bounce copy pipeline layout"),
                    bind_group_layouts: &[Some(&copy_layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("item-bounce shader"),
                source: wgpu::ShaderSource::Wgsl(SHADER.into()),
            });

        self.item_pipeline = Some(context.device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("item-bounce persistent item pipeline"),
                layout: Some(&item_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_item"),
                    buffers: &[Some(Vertex::layout())],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_item"),
                    compilation_options: Default::default(),
                    targets: &[Some(TRAIL_FORMAT.into())],
                }),
                primitive: ccw_backface_primitive(),
                // The Web module explicitly disables depth testing and never enables blend.
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            },
        ));
        self.copy_pipeline = Some(context.device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("item-bounce trail copy pipeline"),
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
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            },
        ));
        self.uniforms = Some(context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("item-bounce uniforms"),
            size: UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.item_layout = Some(item_layout);
        self.copy_layout = Some(copy_layout);
        self.atlas_view = Some(atlas_view);
        self.atlas_sampler = Some(atlas_sampler);
        self._atlas = Some(atlas);
        Ok(())
    }

    fn resize(&mut self, context: &RenderContext<'_>, size: RenderSize) {
        let trail = TrailTarget::new(context.device, size);
        let copy_layout = self
            .copy_layout
            .as_ref()
            .expect("ItemBounceModule was not initialized");
        let sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("item-bounce trail nearest sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        self.copy_bind_group = Some(
            context
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("item-bounce trail copy bind group"),
                    layout: copy_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(&trail.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::Sampler(&sampler),
                        },
                    ],
                }),
        );
        self.trail = Some(trail);
    }

    fn update(&mut self, frame: FrameInfo) {
        // The common Web callback converts the RAF timestamp from milliseconds
        // to seconds before func[112] applies its strict >1/15 s gate. An
        // accepted update advances exactly one tick; delayed frames never catch up.
        let advance =
            self.state
                .advance(frame.elapsed.as_secs_f64(), frame.size, self.models.len());
        self.draw_this_frame = advance.accepted;
        if advance.new_run {
            self.geometry = None;
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
        if self
            .trail
            .as_ref()
            .expect("ItemBounceModule was not resized")
            .clear_pending
        {
            let trail = self
                .trail
                .as_ref()
                .expect("ItemBounceModule was not resized");
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("item-bounce initial trail clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &trail.view,
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
            self.trail
                .as_mut()
                .expect("ItemBounceModule was not resized")
                .clear_pending = false;
        }

        if self.draw_this_frame {
            let run = self
                .state
                .run
                .as_ref()
                .expect("an accepted item-bounce update must have a run");
            if self.geometry.is_none() {
                let vertices = self.models[run.model].vertices();
                self.geometry = Some(Geometry::new(
                    context,
                    self.item_layout
                        .as_ref()
                        .expect("ItemBounceModule was not initialized"),
                    self.uniforms
                        .as_ref()
                        .expect("ItemBounceModule was not initialized"),
                    self.atlas_view
                        .as_ref()
                        .expect("ItemBounceModule was not initialized"),
                    self.atlas_sampler
                        .as_ref()
                        .expect("ItemBounceModule was not initialized"),
                    vertices,
                ));
            }

            // Web demotes run elapsed to f32 before dividing by the f32 Q sample.
            let cycle_time = (self.state.internal_time() - run.started_at) as f32 / run.time_scale;
            context.queue.write_buffer(
                self.uniforms
                    .as_ref()
                    .expect("ItemBounceModule was not initialized"),
                0,
                &uniform_bytes(frame.size, cycle_time, run),
            );

            let trail = self
                .trail
                .as_ref()
                .expect("ItemBounceModule was not resized");
            let geometry = self.geometry.as_ref().expect("geometry was just created");
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("item-bounce persistent trail pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &trail.view,
                    depth_slice: None,
                    resolve_target: None,
                    // Never clear on an animation update or a new run.
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
            pass.set_pipeline(
                self.item_pipeline
                    .as_ref()
                    .expect("ItemBounceModule was not initialized"),
            );
            pass.set_bind_group(0, &geometry.bind_group, &[]);
            pass.set_vertex_buffer(0, geometry.buffer.slice(..));
            pass.draw(0..geometry.vertex_count, 0..1);
        }

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("item-bounce trail copy pass"),
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
            self.copy_pipeline
                .as_ref()
                .expect("ItemBounceModule was not initialized"),
        );
        pass.set_bind_group(
            0,
            self.copy_bind_group
                .as_ref()
                .expect("ItemBounceModule was not resized"),
            &[],
        );
        pass.draw(0..6, 0..1);
    }
}

struct State {
    rng: Hc128Rng,
    accepted_ticks: u64,
    last_accepted_at: Option<f64>,
    run: Option<Run>,
}

impl State {
    fn new() -> Self {
        let mut seed = [0; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        Self::from_rng(Hc128Rng::from_seed(seed))
    }

    fn from_rng(rng: Hc128Rng) -> Self {
        Self {
            rng,
            accepted_ticks: 0,
            last_accepted_at: None,
            run: None,
        }
    }

    fn internal_time(&self) -> f64 {
        self.accepted_ticks as f64 * INTERNAL_TICKS_PER_FRAME
    }

    /// Apply the Web timestamp gate and, when accepted, advance exactly one tick.
    fn advance(&mut self, elapsed: f64, size: RenderSize, model_count: usize) -> Advance {
        if self
            .last_accepted_at
            .is_some_and(|last| elapsed - last <= UPDATE_INTERVAL_SECONDS)
        {
            return Advance {
                accepted: false,
                new_run: false,
            };
        }
        self.last_accepted_at = Some(elapsed);
        self.accepted_ticks = self.accepted_ticks.saturating_add(1);
        let now = self.internal_time();
        let needs_run = self
            .run
            .as_ref()
            .is_none_or(|run| now - run.started_at > run.duration);
        if !needs_run {
            return Advance {
                accepted: true,
                new_run: false,
            };
        }

        let height = size.height.max(1) as f32;
        let width = size.width.max(1) as f32;
        // wasm32 samples a u32 range. Keep that width on 64-bit native so one
        // HC-128 word is consumed and all following f32 draws stay aligned.
        let model = self.rng.gen_range(0_u32, model_count as u32) as usize;
        let amplitude = self.rng.gen_range(height * 0.25, height * 0.8);
        let phase = -self.rng.gen_range(0.0_f32, 1.0);
        let decay = self.rng.gen_range(0.01_f32, 0.2);
        let crossing_time = self.rng.gen_range(2.0_f32, 8.0);
        let time_scale = self.rng.gen_range(1.0_f32, 5.0);
        self.run = Some(Run {
            model,
            started_at: now,
            duration: f64::from(crossing_time * time_scale),
            amplitude,
            phase,
            decay,
            speed: -(width + 64.0) / crossing_time,
            time_scale,
        });
        Advance {
            accepted: true,
            new_run: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Advance {
    accepted: bool,
    new_run: bool,
}

struct Run {
    model: usize,
    started_at: f64,
    duration: f64,
    amplitude: f32,
    phase: f32,
    decay: f32,
    speed: f32,
    time_scale: f32,
}

struct Model {
    name: String,
    vertices: Option<Vec<Vertex>>,
}

impl Model {
    fn vertices(&mut self) -> &[Vertex] {
        if self.vertices.is_none() {
            let source = crate::resources::load_utf8(&self.name)
                .unwrap_or_else(|error| panic!("could not load {}: {error}", self.name));
            let obj = ObjData::load_buf(Cursor::new(source.into_bytes()))
                .unwrap_or_else(|error| panic!("could not parse {}: {error}", self.name));
            self.vertices = Some(deindex_obj(&self.name, &obj));
        }
        self.vertices.as_deref().expect("model was just loaded")
    }
}

struct Geometry {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    vertex_count: u32,
}

impl Geometry {
    fn new(
        context: &RenderContext<'_>,
        layout: &wgpu::BindGroupLayout,
        uniforms: &wgpu::Buffer,
        atlas_view: &wgpu::TextureView,
        atlas_sampler: &wgpu::Sampler,
        vertices: &[Vertex],
    ) -> Self {
        let bytes = vertex_bytes(vertices);
        let buffer = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("item-bounce OBJ VBO"),
            size: bytes.len().max(4) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        context.queue.write_buffer(&buffer, 0, &bytes);
        let bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("item-bounce item bind group"),
                layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(atlas_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(atlas_sampler),
                    },
                ],
            });
        Self {
            buffer,
            bind_group,
            vertex_count: vertices.len() as u32,
        }
    }
}

struct TrailTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    clear_pending: bool,
}

impl TrailTarget {
    fn new(device: &wgpu::Device, size: RenderSize) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("item-bounce persistent trail texture"),
            size: wgpu::Extent3d {
                width: size.width.max(1),
                height: size.height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TRAIL_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Self {
            _texture: texture,
            view,
            clear_pending: true,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    position: [f32; 3],
    uv: [f32; 2],
}

impl Vertex {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        const ATTRIBUTES: [wgpu::VertexAttribute; 2] =
            wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2];
        wgpu::VertexBufferLayout {
            array_stride: 20,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ATTRIBUTES,
        }
    }
}

fn load_manifest() -> Result<Vec<Model>, Box<dyn Error>> {
    let list = crate::resources::load_utf8(MANIFEST_RESOURCE)?;
    let names: Vec<_> = list.lines().filter(|name| !name.is_empty()).collect();
    let unique = names.iter().copied().collect::<HashSet<_>>();
    if names.len() != MODEL_COUNT
        || unique.len() != MODEL_COUNT
        || names.iter().any(|name| !name.starts_with("item_models/"))
    {
        return Err(format!(
            "{MANIFEST_RESOURCE} must contain {MODEL_COUNT} unique item_models paths"
        )
        .into());
    }
    Ok(names
        .into_iter()
        .map(|name| Model {
            name: name.into(),
            vertices: None,
        })
        .collect())
}

fn deindex_obj(name: &str, obj: &ObjData) -> Vec<Vertex> {
    let mut vertices = Vec::new();
    for object in &obj.objects {
        for group in &object.groups {
            for polygon in &group.polys {
                if polygon.0.len() < 3 {
                    panic!("{name}: face has fewer than three vertices");
                }
                for index in 1..polygon.0.len() - 1 {
                    for tuple in [polygon.0[0], polygon.0[index], polygon.0[index + 1]] {
                        let texture = tuple
                            .1
                            .unwrap_or_else(|| panic!("{name}: missing texture index"));
                        let position = obj.position[tuple.0];
                        vertices.push(Vertex {
                            // The Web model loader centers X/Z around the spin
                            // axis but deliberately leaves Y in its source range.
                            position: [position[0] - 0.5, position[1], position[2] - 0.5],
                            uv: obj.texture[texture],
                        });
                    }
                }
            }
        }
    }
    if vertices.is_empty() {
        panic!("{name}: no triangle faces");
    }
    vertices
}

fn vertex_bytes(vertices: &[Vertex]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vertices.len() * 20);
    for vertex in vertices {
        for value in vertex.position.iter().chain(vertex.uv.iter()) {
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
    }
    bytes
}

fn uniform_bytes(size: RenderSize, cycle_time: f32, run: &Run) -> [u8; UNIFORM_BYTES as usize] {
    let width = size.width.max(1) as f32;
    let height = size.height.max(1) as f32;
    // Exact Web VP upload: orthographic pixel coordinates with (0,0) at the
    // lower-right launch point and a harmless Z scale (depth test is disabled).
    let values = [
        2.0 / width,
        0.0,
        0.0,
        0.0,
        0.0,
        2.0 / height,
        0.0,
        0.0,
        0.0,
        0.0,
        0.125,
        0.0,
        1.0,
        -1.0,
        0.0,
        1.0,
        cycle_time,
        run.amplitude,
        run.phase,
        run.decay,
        run.speed,
        0.0,
        0.0,
        0.0,
    ];
    let mut bytes = [0; UNIFORM_BYTES as usize];
    for (index, value) in values.into_iter().enumerate() {
        bytes[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

fn uniform_binding(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX,
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

fn ccw_backface_primitive() -> wgpu::PrimitiveState {
    wgpu::PrimitiveState {
        topology: wgpu::PrimitiveTopology::TriangleList,
        strip_index_format: None,
        front_face: wgpu::FrontFace::Ccw,
        cull_mode: Some(wgpu::Face::Back),
        unclipped_depth: false,
        polygon_mode: wgpu::PolygonMode::Fill,
        conservative: false,
    }
}

const SHADER: &str = r#"
struct Uniforms {
    vp: mat4x4<f32>,
    motion_a: vec4<f32>,
    motion_b: vec4<f32>,
};

struct ItemInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

@vertex
fn vs_item(input: ItemInput) -> VertexOutput {
    let cycle_time = uniforms.motion_a.x;
    let amplitude = uniforms.motion_a.y;
    let phase = uniforms.motion_a.z;
    let decay = uniforms.motion_a.w;
    let speed = uniforms.motion_b.x;
    let angle = cycle_time;
    let sine = sin(angle);
    let cosine = cos(angle);
    let rotated = vec3<f32>(
        cosine * input.position.x - sine * input.position.z,
        input.position.y,
        sine * input.position.x + cosine * input.position.z,
    );
    let x = cycle_time * speed;
    let y = abs(cos(3.14159265358979323846 * cycle_time + phase)
        * amplitude * exp(-decay * cycle_time));

    var output: VertexOutput;
    let web_clip = uniforms.vp * vec4<f32>(
        64.0 * rotated.x + x,
        64.0 * rotated.y + y,
        rotated.z,
        1.0,
    );
    // WebGL clips at -w <= z <= w, while WebGPU clips at 0 <= z <= w.
    // Depth testing is disabled in both APIs, but homogeneous near-plane
    // clipping still applies, so remap the captured Web clip depth explicitly.
    output.position = vec4<f32>(
        web_clip.xy,
        0.5 * (web_clip.z + web_clip.w),
        web_clip.w,
    );
    // WebGL treats the decoded PNG's first row as the texture's lower edge;
    // WGPU treats it as the upper edge, so mirror V only at sampling.
    output.uv = vec2<f32>(input.uv.x, 1.0 - input.uv.y);
    return output;
}

@fragment
fn fs_item(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(atlas, atlas_sampler, input.uv);
    if (color.a < 0.1) {
        discard;
    }
    // Preserve WebGL's numeric RGBA values in the Rgba8Unorm trail.
    return color;
}

struct CopyOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(3) var trail: texture_2d<f32>;
@group(0) @binding(4) var trail_sampler: sampler;

@vertex
fn vs_copy(@builtin(vertex_index) index: u32) -> CopyOutput {
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let texcoords = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 0.0),
    );
    var output: CopyOutput;
    output.position = vec4<f32>(positions[index], 0.0, 1.0);
    output.uv = texcoords[index];
    return output;
}

fn srgb_to_linear(channel: f32) -> f32 {
    if (channel <= 0.04045) {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_copy_srgb(input: CopyOutput) -> @location(0) vec4<f32> {
    let webgl_color = textureSample(trail, trail_sampler, input.uv);
    return vec4<f32>(
        srgb_to_linear(webgl_color.r),
        srgb_to_linear(webgl_color.g),
        srgb_to_linear(webgl_color.b),
        1.0,
    );
}

@fragment
fn fs_copy_unorm(input: CopyOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(trail, trail_sampler, input.uv).rgb, 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand_hc::Hc128Rng;

    use super::{
        INTERNAL_TICKS_PER_FRAME, MODEL_COUNT, RenderSize, State, UPDATE_INTERVAL_SECONDS,
        load_manifest, uniform_bytes,
    };

    #[test]
    fn manifest_and_obj_match_the_web_layout() {
        let mut models = load_manifest().expect("valid item-bounce manifest");
        assert_eq!(models.len(), MODEL_COUNT);
        let vertices = models[0].vertices();
        assert!(!vertices.is_empty());
        assert_eq!(super::vertex_bytes(vertices).len(), vertices.len() * 20);
        assert!(vertices.iter().all(|vertex| {
            (-0.5..=0.5).contains(&vertex.position[0])
                && (0.0..=1.0).contains(&vertex.position[1])
                && (-0.5..=0.5).contains(&vertex.position[2])
        }));
        assert!(vertices.iter().any(|vertex| vertex.position[0] < 0.0));
        assert!(vertices.iter().any(|vertex| vertex.position[2] < 0.0));
    }

    #[test]
    fn model_preprocessing_matches_the_captured_web_bubble_coral_vbo() {
        let mut models = load_manifest().expect("valid item-bounce manifest");
        assert_eq!(models[127].name, "item_models/bubble_coral_block.obj");
        let vertices = models[127].vertices();
        assert_eq!(vertices[0].position, [-0.5, 0.0, 0.5]);
        assert_eq!(vertices[1].position, [-0.5, 0.0, -0.5]);
        assert_eq!(vertices[2].position, [0.5, 0.0, -0.5]);
        let effective_web_uv = |vertex: &super::Vertex| [vertex.uv[0], 1.0 - vertex.uv[1]];
        assert_eq!(effective_web_uv(&vertices[0]), [0.203_156, 0.171_906]);
        assert_eq!(effective_web_uv(&vertices[1]), [0.203_156, 0.187_469]);
        assert_eq!(effective_web_uv(&vertices[2]), [0.218_719, 0.187_469]);
    }

    #[test]
    fn sampled_run_respects_all_web_ranges_and_crossing_identity() {
        let size = RenderSize {
            width: 1_280,
            height: 720,
        };
        let mut state = State::from_rng(Hc128Rng::from_seed([7; 32]));
        let advance = state.advance(0.0, size, MODEL_COUNT);
        assert!(advance.accepted && advance.new_run);
        let run = state.run.as_ref().unwrap();
        // Locks the wasm32-width u32 model draw and all following HC-128 f32
        // draws. Sampling usize here on a 64-bit target shifts this sequence.
        assert_eq!(run.model, 824);
        assert_eq!(run.amplitude, 317.147_74);
        assert_eq!(run.phase, -0.399_992_82);
        assert_eq!(run.decay, 0.028_307_216);
        assert_eq!(run.duration, 4.226_469_993_591_309);
        assert_eq!(run.speed, -623.598_4);
        assert_eq!(run.time_scale, 1.961_026_7);
        assert!((180.0..576.0).contains(&run.amplitude));
        assert!(run.phase <= 0.0 && run.phase > -1.0);
        assert!((0.01..0.2).contains(&run.decay));
        assert!((1.0..5.0).contains(&run.time_scale));
        let crossing_time = run.duration as f32 / run.time_scale;
        assert!((2.0..8.0).contains(&crossing_time));
        assert!((run.speed * crossing_time + 1_344.0).abs() < 0.001);
    }

    #[test]
    fn run_replacement_uses_strict_greater_than_and_no_catch_up() {
        let size = RenderSize {
            width: 1_280,
            height: 720,
        };
        let mut state = State::from_rng(Hc128Rng::from_seed([11; 32]));
        assert!(state.advance(0.0, size, MODEL_COUNT).new_run);
        let started_at = state.run.as_ref().unwrap().started_at;
        let equality_time = 3.0 * INTERNAL_TICKS_PER_FRAME;
        state.run.as_mut().unwrap().duration = equality_time - started_at;
        assert!(!state.advance(0.1, size, MODEL_COUNT).new_run);
        assert!(!state.advance(0.2, size, MODEL_COUNT).new_run);
        assert_eq!(state.run.as_ref().unwrap().started_at, started_at);
        assert!(state.advance(0.3, size, MODEL_COUNT).new_run);
        assert_eq!(state.accepted_ticks, 4);
    }

    #[test]
    fn timestamp_gate_is_strict_and_does_not_catch_up() {
        let size = RenderSize {
            width: 1_280,
            height: 720,
        };
        let mut state = State::from_rng(Hc128Rng::from_seed([12; 32]));
        assert!(state.advance(0.0, size, MODEL_COUNT).accepted);
        assert!(
            !state
                .advance(UPDATE_INTERVAL_SECONDS, size, MODEL_COUNT)
                .accepted
        );
        assert_eq!(state.accepted_ticks, 1);
        assert!(
            state
                .advance(UPDATE_INTERVAL_SECONDS + 1e-9, size, MODEL_COUNT)
                .accepted
        );
        assert_eq!(state.accepted_ticks, 2);
        assert!(state.advance(10.0, size, MODEL_COUNT).accepted);
        assert_eq!(state.accepted_ticks, 3);
    }

    #[test]
    fn webgl_signed_clip_depth_is_remapped_into_webgpu_range() {
        let remap = |web_z: f32, web_w: f32| 0.5 * (web_z + web_w);
        assert_eq!(remap(-0.0625, 1.0), 0.46875);
        assert_eq!(remap(0.0625, 1.0), 0.53125);
        assert!(super::SHADER.contains("0.5 * (web_clip.z + web_clip.w)"));
    }

    #[test]
    fn uniform_layout_contains_the_captured_web_vp_and_motion_values() {
        let size = RenderSize {
            width: 1_280,
            height: 720,
        };
        let mut state = State::from_rng(Hc128Rng::from_seed([13; 32]));
        state.advance(0.0, size, MODEL_COUNT);
        let run = state.run.as_ref().unwrap();
        let bytes = uniform_bytes(size, 0.25, run);
        let value =
            |index: usize| f32::from_ne_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap());
        assert!((value(0) - 2.0 / 1_280.0).abs() < f32::EPSILON);
        assert!((value(5) - 2.0 / 720.0).abs() < f32::EPSILON);
        assert_eq!(value(10), 0.125);
        assert_eq!(value(12), 1.0);
        assert_eq!(value(13), -1.0);
        assert_eq!(value(16), 0.25);
        assert_eq!(value(17), run.amplitude);
        assert_eq!(value(18), run.phase);
        assert_eq!(value(19), run.decay);
        assert_eq!(value(20), run.speed);
    }
}
