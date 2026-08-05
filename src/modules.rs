use std::{error::Error, time::Duration};

mod alpha_fluids;
mod blocks;
mod creeper;
mod dvd_bounce;
mod footprint;
mod grass;
mod item_bounce;
mod item_pop;
mod load_cube;
mod panorama;
mod squid;

pub use alpha_fluids::{AlphaFluidVariant, AlphaFluidsModule};
pub use blocks::BlocksModule;
pub use creeper::CreeperModule;
pub use dvd_bounce::{DvdBounceModule, DvdBounceVariant};
pub use footprint::FootprintModule;
pub use grass::GrassModule;
pub use item_bounce::ItemBounceModule;
pub use item_pop::ItemPopModule;
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

/// Selects whether a surface-writing shader must compensate for the target's
/// automatic sRGB encode. Off-screen numeric-domain passes choose their entry
/// explicitly instead of using this helper.
pub(super) fn web_surface_fragment_entry(
    surface_format: wgpu::TextureFormat,
    srgb_entry: &'static str,
    unorm_entry: &'static str,
) -> &'static str {
    if surface_format.is_srgb() {
        srgb_entry
    } else {
        unorm_entry
    }
}

#[cfg(test)]
mod tests {
    use super::web_surface_fragment_entry;

    #[test]
    fn web_fragment_entry_matches_surface_encoding() {
        assert_eq!(
            web_surface_fragment_entry(wgpu::TextureFormat::Bgra8UnormSrgb, "fs_srgb", "fs_unorm",),
            "fs_srgb"
        );
        assert_eq!(
            web_surface_fragment_entry(wgpu::TextureFormat::Rgba8Unorm, "fs_srgb", "fs_unorm",),
            "fs_unorm"
        );
    }
}
