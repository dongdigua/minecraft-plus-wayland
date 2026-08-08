use crate::{lock::state::LockVisual, modules::RenderContext};

const COLOR_BYTES: u64 = 16;

pub struct TriangleAnimation {
    format: Option<wgpu::TextureFormat>,
    pipeline: Option<wgpu::RenderPipeline>,
    bind_group: Option<wgpu::BindGroup>,
    color: Option<wgpu::Buffer>,
}

impl TriangleAnimation {
    pub fn new() -> Self {
        Self {
            format: None,
            pipeline: None,
            bind_group: None,
            color: None,
        }
    }

    pub fn ensure_initialized(&mut self, context: &RenderContext<'_>) {
        if self.format == Some(context.surface_format) {
            return;
        }
        let layout = context
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("lock triangle bind group layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
        let pipeline_layout =
            context
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("lock triangle pipeline layout"),
                    bind_group_layouts: &[Some(&layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("lock triangle shader"),
                source: wgpu::ShaderSource::Wgsl(TRIANGLE_SHADER.into()),
            });
        let color = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lock triangle color"),
            size: COLOR_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lock triangle bind group"),
                layout: &layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: color.as_entire_binding(),
                }],
            });
        let pipeline = context
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("lock triangle pipeline"),
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
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: Some(wgpu::Face::Back),
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        self.format = Some(context.surface_format);
        self.pipeline = Some(pipeline);
        self.bind_group = Some(bind_group);
        self.color = Some(color);
    }

    pub fn draw(
        &mut self,
        context: &RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        visual: LockVisual,
    ) {
        let rgba = match visual {
            LockVisual::Hidden | LockVisual::Torch { .. } => return,
            // Endpoint-only RGB primaries remain numerically identical on UNORM and sRGB
            // surfaces, avoiding a second color-space policy for these retained overlays.
            LockVisual::AuthenticatingYellow => [1.0, 1.0, 0.0, 1.0],
            LockVisual::FailedRed => [1.0, 0.0, 0.0, 1.0],
            LockVisual::AuthenticatedGreen { .. } => [0.0, 1.0, 0.0, 1.0],
        };
        self.ensure_initialized(context);
        let color = self.color.as_ref().expect("triangle color initialized");
        context.queue.write_buffer(color, 0, &rgba_bytes(rgba));

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("lock status triangle"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
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
            self.pipeline
                .as_ref()
                .expect("triangle pipeline initialized"),
        );
        pass.set_bind_group(
            0,
            self.bind_group
                .as_ref()
                .expect("triangle bind group initialized"),
            &[],
        );
        pass.draw(0..3, 0..1);
    }
}

impl Default for TriangleAnimation {
    fn default() -> Self {
        Self::new()
    }
}

fn rgba_bytes(values: [f32; 4]) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    for (chunk, value) in bytes.chunks_exact_mut(4).zip(values) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

const TRIANGLE_SHADER: &str = r#"
struct Color {
    value: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> color: Color;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>( 0.0,  0.35),
        vec2<f32>(-0.30, -0.25),
        vec2<f32>( 0.30, -0.25),
    );
    return vec4<f32>(positions[index], 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return color.value;
}
"#;

#[cfg(test)]
mod tests {
    use super::rgba_bytes;

    #[test]
    fn color_uniform_has_exact_vec4_layout() {
        let bytes = rgba_bytes([1.0, 0.5, 0.25, 1.0]);
        assert_eq!(&bytes[0..4], &1.0_f32.to_ne_bytes());
        assert_eq!(&bytes[12..16], &1.0_f32.to_ne_bytes());
    }
}
