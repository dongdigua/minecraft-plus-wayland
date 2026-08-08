use std::{error::Error, time::Instant};

use crate::{
    lock::state::{CREEPER_APPROACH_DURATION, DISSOLVE_DURATION, LockVisual},
    modules::{RenderContext, RenderSize},
};

const SHADER: &str = include_str!("creeper.wgsl");
const CREEPER_RESOURCE: &str = "creeper.png";
const CREEPER_DIMENSION: u32 = 8;
const UNIFORM_BYTES: u64 = 32;

pub struct CreeperAnimation {
    format: Option<wgpu::TextureFormat>,
    pipeline: Option<wgpu::RenderPipeline>,
    bind_group: Option<wgpu::BindGroup>,
    uniforms: Option<wgpu::Buffer>,
}

impl CreeperAnimation {
    pub fn new() -> Self {
        Self {
            format: None,
            pipeline: None,
            bind_group: None,
            uniforms: None,
        }
    }

    pub fn ensure_initialized(
        &mut self,
        context: &RenderContext<'_>,
    ) -> Result<(), Box<dyn Error>> {
        if self.format == Some(context.surface_format) {
            return Ok(());
        }

        let creeper = crate::resources::load_rgba_png(CREEPER_RESOURCE)?;
        if creeper.dimensions() != (CREEPER_DIMENSION, CREEPER_DIMENSION) {
            return Err(format!(
                "{CREEPER_RESOURCE} must be {CREEPER_DIMENSION}x{CREEPER_DIMENSION}, got {}x{}",
                creeper.width(),
                creeper.height()
            )
            .into());
        }
        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lock creeper texture"),
            size: wgpu::Extent3d {
                width: CREEPER_DIMENSION,
                height: CREEPER_DIMENSION,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Match module 12 and the Web numeric texture domain.
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
            creeper.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(CREEPER_DIMENSION * 4),
                rows_per_image: Some(CREEPER_DIMENSION),
            },
            texture.size(),
        );
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("lock creeper sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let uniforms = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lock creeper animation uniforms"),
            size: UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group_layout =
            context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("lock creeper bind group layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
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
                            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ],
                });
        let bind_group = context
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lock creeper bind group"),
                layout: &bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: uniforms.as_entire_binding(),
                    },
                ],
            });
        let pipeline_layout =
            context
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("lock creeper pipeline layout"),
                    bind_group_layouts: &[Some(&bind_group_layout)],
                    immediate_size: 0,
                });
        let shader = context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("lock creeper animation shader"),
                source: wgpu::ShaderSource::Wgsl(SHADER.into()),
            });
        let fragment_entry = if context.surface_format.is_srgb() {
            "fs_srgb"
        } else {
            "fs_unorm"
        };
        let pipeline = context
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("lock creeper animation pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fragment_entry),
                    compilation_options: Default::default(),
                    targets: &[Some(context.surface_format.into())],
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
        self.uniforms = Some(uniforms);
        Ok(())
    }

    pub fn draw(
        &mut self,
        context: &RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        size: RenderSize,
        visual: LockVisual,
        frame_time: Instant,
    ) {
        let (approach_progress, dissolve_progress, red, draw_head) = match visual {
            LockVisual::Creeper {
                approach_started_at,
                red,
            } => (
                normalized_progress(
                    frame_time.saturating_duration_since(approach_started_at),
                    CREEPER_APPROACH_DURATION,
                ),
                0.0,
                red,
                true,
            ),
            LockVisual::DissolvingCreeper {
                approach_started_at,
                started_at,
                ..
            } => (
                normalized_progress(
                    frame_time.saturating_duration_since(approach_started_at),
                    CREEPER_APPROACH_DURATION,
                ),
                normalized_progress(
                    frame_time.saturating_duration_since(started_at),
                    DISSOLVE_DURATION,
                ),
                false,
                true,
            ),
            LockVisual::FatalBlack => (0.0, 0.0, false, false),
            LockVisual::Hidden | LockVisual::Torch { .. } => return,
        };
        self.ensure_initialized(context)
            .expect("creeper animation initialized during surface configure");
        let uniforms = self
            .uniforms
            .as_ref()
            .expect("lock creeper uniforms initialized");
        context.queue.write_buffer(
            uniforms,
            0,
            &uniform_bytes(
                size,
                approach_progress,
                dissolve_progress,
                red,
                draw_head && dissolve_progress > 0.0,
            ),
        );

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("lock creeper black scene and animation"),
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
        if draw_head {
            pass.set_pipeline(
                self.pipeline
                    .as_ref()
                    .expect("lock creeper pipeline initialized"),
            );
            pass.set_bind_group(
                0,
                self.bind_group
                    .as_ref()
                    .expect("lock creeper bind group initialized"),
                &[],
            );
            pass.draw(0..6, 0..1);
        }
    }
}

impl Default for CreeperAnimation {
    fn default() -> Self {
        Self::new()
    }
}

fn normalized_progress(elapsed: std::time::Duration, duration: std::time::Duration) -> f32 {
    (elapsed.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
}

fn uniform_bytes(
    size: RenderSize,
    approach_progress: f32,
    dissolve_progress: f32,
    red: bool,
    dissolving: bool,
) -> [u8; UNIFORM_BYTES as usize] {
    let mut bytes = [0; UNIFORM_BYTES as usize];
    for (index, value) in [
        size.width.max(1) as f32,
        size.height.max(1) as f32,
        approach_progress,
        dissolve_progress,
    ]
    .into_iter()
    .enumerate()
    {
        let offset = index * 4;
        bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes[16..20].copy_from_slice(&u32::from(red).to_ne_bytes());
    bytes[20..24].copy_from_slice(&u32::from(dissolving).to_ne_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn shader_parses_and_validates() {
        let module = wgpu::naga::front::wgsl::parse_str(SHADER).expect("creeper WGSL parses");
        wgpu::naga::valid::Validator::new(
            wgpu::naga::valid::ValidationFlags::all(),
            wgpu::naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("creeper WGSL validates");
    }

    #[test]
    fn progress_is_clamped_to_the_shared_timeline() {
        assert_eq!(
            normalized_progress(Duration::ZERO, CREEPER_APPROACH_DURATION),
            0.0
        );
        assert_eq!(
            normalized_progress(CREEPER_APPROACH_DURATION / 2, CREEPER_APPROACH_DURATION),
            0.5
        );
        assert_eq!(
            normalized_progress(CREEPER_APPROACH_DURATION * 2, CREEPER_APPROACH_DURATION),
            1.0
        );
    }

    #[test]
    fn instant_success_approaches_while_the_first_half_dissolves() {
        let elapsed = CREEPER_APPROACH_DURATION;
        assert_eq!(normalized_progress(elapsed, CREEPER_APPROACH_DURATION), 1.0);
        assert_eq!(normalized_progress(elapsed, DISSOLVE_DURATION), 0.5);
    }

    #[test]
    fn uniforms_follow_the_wgsl_layout() {
        let bytes = uniform_bytes(
            RenderSize {
                width: 1920,
                height: 1080,
            },
            0.25,
            0.75,
            true,
            true,
        );
        assert_eq!(bytes.len(), UNIFORM_BYTES as usize);
        assert_eq!(&bytes[0..4], &1920.0_f32.to_ne_bytes());
        assert_eq!(&bytes[4..8], &1080.0_f32.to_ne_bytes());
        assert_eq!(&bytes[8..12], &0.25_f32.to_ne_bytes());
        assert_eq!(&bytes[12..16], &0.75_f32.to_ne_bytes());
        assert_eq!(&bytes[16..20], &1_u32.to_ne_bytes());
        assert_eq!(&bytes[20..24], &1_u32.to_ne_bytes());
        assert!(bytes[24..].iter().all(|byte| *byte == 0));
    }
}
