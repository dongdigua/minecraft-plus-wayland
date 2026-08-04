use std::{error::Error, time::Duration};

mod alpha_fluids;
mod creeper;
mod grass;
mod load_cube;
mod panorama;
mod squid;

pub use alpha_fluids::{AlphaFluidVariant, AlphaFluidsModule};
pub use creeper::CreeperModule;
pub use grass::GrassModule;
pub use load_cube::LoadCubeModule;
pub use panorama::PanoramaModule;
pub use squid::SquidModule;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    pub elapsed: Duration,
    pub delta: Duration,
    pub size: RenderSize,
}

pub struct RenderContext<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub surface_format: wgpu::TextureFormat,
}

pub trait Module: 'static {
    fn initialize(&mut self, _context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        Ok(())
    }

    fn resize(&mut self, _context: &RenderContext<'_>, _size: RenderSize) {}

    fn update(&mut self, _frame: FrameInfo) {}

    fn wants_continuous_frames(&self) -> bool {
        false
    }

    fn render(
        &mut self,
        context: &RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        frame: FrameInfo,
    );
}

#[derive(Default)]
pub struct TriangleModule {
    pipeline: Option<wgpu::RenderPipeline>,
}

impl Module for TriangleModule {
    fn initialize(&mut self, context: &RenderContext<'_>) -> Result<(), Box<dyn Error>> {
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("triangle shader"),
                source: wgpu::ShaderSource::Wgsl(
                    r#"
                    @vertex
                    fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
                        let positions = array<vec2<f32>, 3>(
                            vec2<f32>(0.0, 0.62),
                            vec2<f32>(-0.62, -0.62),
                            vec2<f32>(0.62, -0.62),
                        );
                        return vec4<f32>(positions[index], 0.0, 1.0);
                    }

                    @fragment
                    fn fs_main() -> @location(0) vec4<f32> {
                        return vec4<f32>(0.24, 0.82, 0.18, 1.0);
                    }
                "#
                    .into(),
                ),
            });
        let layout = context
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("triangle pipeline layout"),
                bind_group_layouts: &[],
                immediate_size: 0,
            });

        self.pipeline = Some(context.device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("triangle pipeline"),
                layout: Some(&layout),
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
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            },
        ));
        Ok(())
    }

    fn render(
        &mut self,
        _context: &RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        _frame: FrameInfo,
    ) {
        let pipeline = self
            .pipeline
            .as_ref()
            .expect("TriangleModule was not initialized");
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("triangle module"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(background_color()),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(pipeline);
        pass.draw(0..3, 0..1);
    }
}

pub(super) fn background_color() -> wgpu::Color {
    wgpu::Color {
        r: 0.015,
        g: 0.025,
        b: 0.04,
        a: 1.0,
    }
}
