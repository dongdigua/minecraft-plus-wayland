use super::{FrameInfo, Module, RenderContext, RenderSize, web_surface_fragment_entry};
use obj::ObjData;
use rand::{Rng, RngCore, SeedableRng, distributions::StandardNormal};
use rand_hc::Hc128Rng;
use std::{error::Error, io::Cursor};

const MAX_ITEMS: usize = 10;
const TICKS: f64 = 3.0;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const ATLAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Web module=3: randomly selected OBJ items fly, land, bob/rotate, then
/// expire after 90 Web ticks (30 seconds).
pub struct ItemPopModule {
    pipeline: Option<wgpu::RenderPipeline>,
    layout: Option<wgpu::BindGroupLayout>,
    view: Option<wgpu::TextureView>,
    sampler: Option<wgpu::Sampler>,
    atlas: Option<wgpu::Texture>,
    depth: Option<DepthTarget>,
    models: Vec<Model>,
    state: State,
}
impl Default for ItemPopModule {
    fn default() -> Self {
        Self {
            pipeline: None,
            layout: None,
            view: None,
            sampler: None,
            atlas: None,
            depth: None,
            models: Vec::new(),
            state: State::new(),
        }
    }
}

impl Module for ItemPopModule {
    fn initialize(&mut self, c: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let image = crate::resources::load_rgba_png("item_models/atlas.png")?;
        if image.dimensions() != (1024, 1024) {
            return Err("item atlas must be 1024x1024".into());
        }
        self.models = load_manifest()?;
        if self.models.len() != 1044 {
            return Err(format!(
                "pop_items.txt must contain 1044 models, got {}",
                self.models.len()
            )
            .into());
        }
        let atlas = c.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("item-pop atlas"),
            size: wgpu::Extent3d {
                width: 1024,
                height: 1024,
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
        c.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            image.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4096),
                rows_per_image: Some(1024),
            },
            atlas.size(),
        );
        let view = atlas.create_view(&Default::default());
        let sampler = c.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("item-pop nearest sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let layout = c
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("item-pop layout"),
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
        let shader = c.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("item-pop shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pl = c
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("item-pop pipeline layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
        self.pipeline = Some(
            c.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("item-pop pipeline"),
                    layout: Some(&pl),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        buffers: &[Some(Vertex::layout())],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(web_surface_fragment_entry(
                            c.surface_format,
                            "fs_srgb",
                            "fs_unorm",
                        )),
                        compilation_options: Default::default(),
                        targets: &[Some(c.surface_format.into())],
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        front_face: wgpu::FrontFace::Ccw,
                        cull_mode: Some(wgpu::Face::Back),
                        ..Default::default()
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: DEPTH,
                        depth_write_enabled: Some(true),
                        depth_compare: Some(wgpu::CompareFunction::Less),
                        stencil: Default::default(),
                        bias: Default::default(),
                    }),
                    multisample: Default::default(),
                    multiview_mask: None,
                    cache: None,
                }),
        );
        self.layout = Some(layout);
        self.view = Some(view);
        self.sampler = Some(sampler);
        self.atlas = Some(atlas);
        Ok(())
    }
    fn resize(&mut self, c: &RenderContext<'_>, size: RenderSize) {
        self.depth = Some(DepthTarget::new(c.device, size));
    }
    fn update(&mut self, f: FrameInfo) {
        self.state
            .advance(f.elapsed.as_secs_f64(), self.models.len());
    }
    fn wants_continuous_frames(&self) -> bool {
        true
    }
    fn render(
        &mut self,
        c: &RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        f: FrameInfo,
    ) {
        let (pipeline, layout, view, sampler, depth) = (
            self.pipeline.as_ref().expect("not initialized"),
            self.layout.as_ref().expect("not initialized"),
            self.view.as_ref().expect("not initialized"),
            self.sampler.as_ref().expect("not initialized"),
            self.depth.as_ref().expect("not resized"),
        );
        for i in &mut self.state.items {
            if i.geometry.is_none() {
                i.geometry = Some(Geometry::new(
                    c,
                    layout,
                    view,
                    sampler,
                    self.models[i.model].vertices(),
                ));
            }
        }
        let elapsed = f.elapsed.as_secs_f32();
        for i in &self.state.items {
            let g = i.geometry.as_ref().unwrap();
            c.queue.write_buffer(
                &g.uniform,
                0,
                &matrix_bytes(i.mvp(elapsed, f.size, self.state.camera_height)),
            );
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("item-pop pass"),
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
        for i in &self.state.items {
            let g = i.geometry.as_ref().unwrap();
            pass.set_bind_group(0, &g.group, &[]);
            pass.set_vertex_buffer(0, g.buffer.slice(..));
            pass.draw(0..g.count, 0..1);
        }
    }
}

struct State {
    rng: Hc128Rng,
    // The Web initializer samples Uniform([3, 8)) for the orbit camera Y.
    camera_height: f32,
    last: u64,
    items: Vec<Item>,
}
impl State {
    fn new() -> Self {
        let mut s = [0; 32];
        rand::thread_rng().fill_bytes(&mut s);
        let mut rng = Hc128Rng::from_seed(s);
        let camera_height = rng.gen_range(3.0f32, 8.0);
        Self {
            rng,
            camera_height,
            last: 0,
            items: Vec::new(),
        }
    }
    fn advance(&mut self, seconds: f64, count: usize) {
        let t = seconds.max(0.0) * TICKS;
        // `$f104` invokes its age filter on every RAF before both item motion
        // and the bucket-gated spawn candidate. Do not defer this to a new
        // bucket: an item is gone on the first frame where age is > 90.
        self.items.retain(|i| t - i.birth <= 90.0);
        for i in &mut self.items {
            i.advance(t)
        }
        let b = t.floor() as u64;
        if b == self.last {
            return;
        }
        self.last = b;
        if self.items.len() >= MAX_ITEMS || self.rng.r#gen::<f64>() >= 0.1 {
            return;
        }
        let model = self.rng.gen_range(0, count);
        let x: f64 = self.rng.sample(StandardNormal);
        let y = self.rng.gen_range(1.0f32, 3.0);
        let z: f64 = self.rng.sample(StandardNormal);
        self.items.push(Item {
            model,
            birth: t,
            x: x as f32,
            y,
            z: z as f32,
            phase: Phase::Flying,
            geometry: None,
        });
    }
}
struct Item {
    model: usize,
    birth: f64,
    x: f32,
    y: f32,
    z: f32,
    phase: Phase,
    geometry: Option<Geometry>,
}
enum Phase {
    Flying,
    Grounded { landed: f64, position: [f32; 3] },
}
impl Item {
    fn advance(&mut self, t: f64) {
        if let Phase::Flying = self.phase {
            let a = (t - self.birth) as f32;
            let p = [self.x * a, 1.0 + self.y * a - 0.25 * a * a, self.z * a];
            if p[1] <= 0.0 {
                self.phase = Phase::Grounded {
                    landed: t,
                    position: p,
                }
            }
        }
    }
    fn mvp(&self, seconds: f32, size: RenderSize, camera_height: f32) -> [f32; 16] {
        let t = f64::from(seconds.max(0.0)) * TICKS;
        let (p, angle) = match self.phase {
            Phase::Flying => {
                let a = (t - self.birth) as f32;
                (
                    [self.x * a, 1.0 + self.y * a - 0.25 * a * a, self.z * a],
                    0.0,
                )
            }
            Phase::Grounded { landed, position } => {
                let a = (t - landed) as f32;
                (
                    [
                        position[0],
                        position[1] + (a / 10.0 + 0.1).sin(),
                        position[2],
                    ],
                    a * 15.0_f32.to_radians(),
                )
            }
        };
        mul(
            camera(seconds, size, camera_height),
            mul(translate(p), rot_y(angle)),
        )
    }
}
struct Geometry {
    buffer: wgpu::Buffer,
    uniform: wgpu::Buffer,
    group: wgpu::BindGroup,
    count: u32,
}
impl Geometry {
    fn new(
        c: &RenderContext<'_>,
        layout: &wgpu::BindGroupLayout,
        view: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
        vertices: &[Vertex],
    ) -> Self {
        let bytes = vertex_bytes(vertices);
        let buffer = c.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("item-pop OBJ VBO"),
            size: bytes.len().max(4) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        c.queue.write_buffer(&buffer, 0, &bytes);
        let uniform = c.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("item-pop MVP"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let group = c.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("item-pop item group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        Self {
            buffer,
            uniform,
            group,
            count: vertices.len() as u32,
        }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    position: [f32; 3],
    uv: [f32; 2],
    normal: [f32; 3],
}
impl Vertex {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        const A: [wgpu::VertexAttribute; 3] =
            wgpu::vertex_attr_array![0=>Float32x3,1=>Float32x2,2=>Float32x3];
        wgpu::VertexBufferLayout {
            array_stride: 32,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &A,
        }
    }
}
struct DepthTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}
impl DepthTarget {
    fn new(d: &wgpu::Device, s: RenderSize) -> Self {
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("item-pop depth"),
            size: wgpu::Extent3d {
                width: s.width.max(1),
                height: s.height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Self {
            _texture: texture,
            view,
        }
    }
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

fn load_manifest() -> Result<Vec<Model>, Box<dyn Error>> {
    let list = crate::resources::load_utf8("pop_items.txt")?;
    let names: Vec<_> = list.lines().filter(|name| !name.is_empty()).collect();
    let unique = names.iter().collect::<std::collections::HashSet<_>>();
    if names.len() != 1044
        || unique.len() != names.len()
        || names.iter().any(|name| !name.starts_with("item_models/"))
    {
        return Err("invalid pop_items.txt manifest".into());
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
                        let normal = tuple
                            .2
                            .unwrap_or_else(|| panic!("{name}: missing normal index"));
                        let position = obj.position[tuple.0];
                        vertices.push(Vertex {
                            position: [position[0] - 0.5, position[1], position[2] - 0.5],
                            // Preserve the OBJ UV. The shader performs the sole
                            // WebGL-to-WGPU V-axis mirror at sampling time.
                            uv: obj.texture[texture],
                            normal: obj.normal[normal],
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
fn vertex_bytes(v: &[Vertex]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 32);
    for x in v {
        for f in x.position.iter().chain(x.uv.iter()).chain(x.normal.iter()) {
            b.extend_from_slice(&f.to_ne_bytes())
        }
    }
    b
}
fn matrix_bytes(m: [f32; 16]) -> [u8; 64] {
    let mut b = [0; 64];
    for (i, x) in m.iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&x.to_ne_bytes())
    }
    b
}
fn camera(t: f32, s: RenderSize, height: f32) -> [f32; 16] {
    // Captured Web setup: eye=(20*cos(5°*t), Uniform([3,8)),
    // -20*sin(5°*t)), target=(0,0,0), up=(0,1,0).
    let a = t * 5.0_f32.to_radians();
    mul(
        perspective(
            70f32.to_radians(),
            s.width.max(1) as f32 / s.height.max(1) as f32,
            0.001,
            50.0,
        ),
        look_at(
            [20.0 * a.cos(), height, -20.0 * a.sin()],
            [0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
        ),
    )
}
fn perspective(f: f32, a: f32, n: f32, z: f32) -> [f32; 16] {
    // This is already a WebGPU [0,w] clip-depth projection, not a verbatim
    // WebGL [-w,w] matrix: view-space near maps to z=0 and far maps to z=w.
    let q = 1. / (f * 0.5).tan();
    [
        q / a,
        0.,
        0.,
        0.,
        0.,
        q,
        0.,
        0.,
        0.,
        0.,
        z / (z - n),
        1.,
        0.,
        0.,
        -n * z / (z - n),
        0.,
    ]
}
fn translate(p: [f32; 3]) -> [f32; 16] {
    [
        1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., p[0], p[1], p[2], 1.,
    ]
}
fn rot_y(a: f32) -> [f32; 16] {
    let (s, c) = a.sin_cos();
    [c, 0., -s, 0., 0., 1., 0., 0., s, 0., c, 0., 0., 0., 0., 1.]
}
fn mul(a: [f32; 16], b: [f32; 16]) -> [f32; 16] {
    let mut r = [0.; 16];
    for c in 0..4 {
        for x in 0..4 {
            r[c * 4 + x] = (0..4).map(|i| a[i * 4 + x] * b[c * 4 + i]).sum()
        }
    }
    r
}
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn norm(a: [f32; 3]) -> [f32; 3] {
    let n = dot(a, a).sqrt();
    [a[0] / n, a[1] / n, a[2] / n]
}
fn look_at(e: [f32; 3], target: [f32; 3], up: [f32; 3]) -> [f32; 16] {
    let f = norm(sub(target, e));
    // This order reproduces the captured Web f237 look-at matrix. Reversing
    // either cross product mirrors the item perspective horizontally.
    let r = norm(cross(f, up));
    let u = cross(r, f);
    [
        r[0],
        u[0],
        f[0],
        0.,
        r[1],
        u[1],
        f[1],
        0.,
        r[2],
        u[2],
        f[2],
        0.,
        -dot(r, e),
        -dot(u, e),
        -dot(f, e),
        1.,
    ]
}
const SHADER: &str = r#"
struct Uniforms {
    mvp: mat4x4<f32>,
};

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) normal: vec3<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) light: f32,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = uniforms.mvp * vec4<f32>(input.position, 1.0);
    output.uv = input.uv;
    output.light = 0.4 + 0.6 * max(
        dot(input.normal, normalize(vec3<f32>(2.0, 2.0, 3.0))),
        0.0,
    );
    return output;
}

fn srgb_to_linear(channel: f32) -> f32 {
    if (channel <= 0.04045) {
        return channel / 12.92;
    }
    return pow((channel + 0.055) / 1.055, 2.4);
}

fn atlas_color(input: VertexOutput) -> vec4<f32> {
    // WebGL's raw RGBA upload addresses the PNG's first row at its lower
    // texture edge. WGPU addresses it at its upper edge, so mirror V at the
    // sampling boundary while preserving the OBJ's original UV data.
    return textureSample(atlas, atlas_sampler, vec2<f32>(input.uv.x, 1.0 - input.uv.y));
}

@fragment
fn fs_srgb(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = atlas_color(input);
    if (color.a < 0.1) {
        discard;
    }
    // Lighting is part of the Web framebuffer value. Convert only after the
    // texture and lighting terms have produced that final numeric RGB.
    let web_rgb = color.rgb * input.light;
    return vec4<f32>(
        vec3<f32>(
            srgb_to_linear(web_rgb.r),
            srgb_to_linear(web_rgb.g),
            srgb_to_linear(web_rgb.b),
        ),
        color.a,
    );
}

@fragment
fn fs_unorm(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = atlas_color(input);
    if (color.a < 0.1) {
        discard;
    }
    // The Web frame tail clears only the drawing-buffer alpha to 1 after all
    // item draws. No later module-3 pass observes the intermediate alpha, so
    // writing 1 here is final-frame equivalent and also keeps premultiplied
    // Wayland presentation opaque without an extra pass.
    return vec4<f32>(color.rgb * input.light, 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use obj::ObjData;

    use super::load_manifest;

    #[test]
    fn obj_crate_parses_a_manifest_model() {
        let mut models = load_manifest().expect("valid item-pop manifest");
        assert_eq!(models.len(), 1_044);
        assert!(!models[0].vertices().is_empty());
    }

    #[test]
    fn resource_obj_expansion_centers_xz_without_changing_y_or_uv() {
        let mut models = load_manifest().expect("valid item-pop manifest");
        let source = crate::resources::load_utf8(&models[0].name).expect("manifest OBJ exists");
        let obj = ObjData::load_buf(Cursor::new(source.into_bytes())).expect("valid manifest OBJ");
        let tuple = obj.objects[0].groups[0].polys[0].0[0];
        let source_position = obj.position[tuple.0];
        let source_uv = obj.texture[tuple.1.expect("textured OBJ vertex")];
        let expanded = models[0].vertices()[0];

        assert_eq!(
            expanded.position,
            [
                source_position[0] - 0.5,
                source_position[1],
                source_position[2] - 0.5,
            ]
        );
        assert_eq!(expanded.uv, source_uv);
        assert!(super::SHADER.contains("1.0 - input.uv.y"));
    }

    #[test]
    fn expiry_runs_on_the_first_raf_past_the_limit_even_in_the_same_bucket() {
        let mut state = super::State::new();
        state.last = 90;
        state.items.push(super::Item {
            model: 0,
            birth: 0.0,
            x: 0.0,
            y: 1.0,
            z: 0.0,
            phase: super::Phase::Flying,
            geometry: None,
        });
        // T=90.03 has the same floor(T) bucket, but Web `$f287` still removes it.
        state.advance(30.01, 1);
        assert!(state.items.is_empty());
    }

    #[test]
    fn camera_orientation_matches_the_captured_web_mvp_xy_terms() {
        let matrix = super::camera(
            0.34,
            super::RenderSize {
                width: 1_280,
                height: 720,
            },
            7.628_925_3,
        );
        // Web mock capture at 340 ms; Z is deliberately remapped for WGPU.
        assert!((matrix[0] - -0.023_831_88).abs() < 0.000_001);
        assert!((matrix[1] - -0.508_765_4).abs() < 0.000_001);
        assert!((matrix[8] - -0.802_979_65).abs() < 0.000_001);
        assert!((matrix[9] - 0.015_099_806).abs() < 0.000_001);
    }

    #[test]
    fn perspective_maps_near_and_far_to_webgpu_clip_depth() {
        let near = 0.001;
        let far = 50.0;
        let matrix = super::perspective(70f32.to_radians(), 16.0 / 9.0, near, far);
        let clip_depth = |view_z: f32| {
            let clip_z = matrix[10] * view_z + matrix[14];
            let clip_w = matrix[11] * view_z + matrix[15];
            (clip_z, clip_w)
        };
        let (near_z, near_w) = clip_depth(near);
        let (far_z, far_w) = clip_depth(far);
        assert!(near_z.abs() < 1e-7);
        assert!((far_z - far_w).abs() < 1e-5);
        assert!(near_w > 0.0 && far_w > near_w);
    }

    #[test]
    fn atlas_samples_need_the_webgl_to_wgpu_v_mirror() {
        let mut models = load_manifest().expect("valid item-pop manifest");
        let vertices = models[0].vertices();
        let atlas = crate::resources::load_rgba_png("item_models/atlas.png").unwrap();
        let alpha_at = |uv: [f32; 2], v: f32| {
            let x = (uv[0] * 1024.0).floor().clamp(0.0, 1023.0) as u32;
            let y = (v * 1024.0).floor().clamp(0.0, 1023.0) as u32;
            atlas.get_pixel(x, y)[3]
        };
        assert!(
            vertices
                .iter()
                .all(|vertex| alpha_at(vertex.uv, vertex.uv[1]) < 26)
        );
        assert!(
            vertices
                .iter()
                .any(|vertex| alpha_at(vertex.uv, 1.0 - vertex.uv[1]) >= 26)
        );
    }
}
