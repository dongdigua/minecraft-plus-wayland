use std::{error::Error, fmt, ptr::NonNull};

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, RawDisplayHandle, RawWindowHandle,
    WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::reexports::client::{
    Connection, Proxy, protocol::wl_surface::WlSurface,
};

use crate::{
    lock::{animations::TriangleAnimation, state::LockVisual},
    modules::{FrameInfo, Module, RenderContext, RenderSize},
};

type RendererResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderOutcome {
    Presented,
    Skipped,
}

struct WgpuWaylandDisplay {
    connection: Connection,
}

impl fmt::Debug for WgpuWaylandDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WgpuWaylandDisplay")
    }
}

impl HasDisplayHandle for WgpuWaylandDisplay {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let display = NonNull::new(self.connection.backend().display_ptr() as *mut _)
            .ok_or(HandleError::Unavailable)?;
        let raw = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display));

        // The Connection retained by this provider owns the wl_display for this handle's lifetime.
        unsafe { Ok(DisplayHandle::borrow_raw(raw)) }
    }
}

pub struct Renderer {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: Option<wgpu::SurfaceConfiguration>,
    raw_display_handle: RawDisplayHandle,
    raw_window_handle: RawWindowHandle,
}

impl Renderer {
    pub fn new(connection: &Connection, surface: &WlSurface) -> RendererResult<Self> {
        let raw_display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(connection.backend().display_ptr() as *mut _)
                .expect("Wayland connection has no display pointer"),
        ));
        let raw_window_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(surface.id().as_ptr() as *mut _)
                .expect("Wayland surface has no object pointer"),
        ));

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(WgpuWaylandDisplay {
                connection: connection.clone(),
            }),
        ));
        let surface = Self::create_surface(&instance, raw_display_handle, raw_window_handle)?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("minecraft-plus-wayland device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
            }))?;

        Ok(Self {
            instance,
            surface,
            adapter,
            device,
            queue,
            config: None,
            raw_display_handle,
            raw_window_handle,
        })
    }

    pub fn context(&self) -> RenderContext<'_> {
        RenderContext {
            device: &self.device,
            queue: &self.queue,
            surface_format: self
                .config
                .as_ref()
                .map(|config| config.format)
                .unwrap_or(wgpu::TextureFormat::Bgra8Unorm),
        }
    }

    pub fn configure(&mut self, size: RenderSize) -> RendererResult<()> {
        let capabilities = self.surface.get_capabilities(&self.adapter);
        let mut config = self
            .surface
            .get_default_config(&self.adapter, size.width.max(1), size.height.max(1))
            .ok_or("the selected adapter does not support the layer surface")?;
        config.format = preferred_surface_format(&capabilities.formats)
            .ok_or("the selected adapter reports no supported surface formats")?;
        #[cfg(debug_assertions)]
        if let Some(value) = std::env::var_os("MINECRAFT_PLUS_SURFACE_FORMAT") {
            let value = value
                .into_string()
                .map_err(|_| "MINECRAFT_PLUS_SURFACE_FORMAT is not valid UTF-8")?;
            config.format = debug_surface_format_override(&value, &capabilities.formats)
                .map_err(|error| format!("invalid MINECRAFT_PLUS_SURFACE_FORMAT: {error}"))?;
            log::warn!(
                target: "minecraft_plus_wayland::surface",
                "using debug-only surface format override: value={value:?}, format={:?}, is_srgb={}",
                config.format,
                config.format.is_srgb(),
            );
        }
        if capabilities
            .present_modes
            .contains(&wgpu::PresentMode::Fifo)
        {
            config.present_mode = wgpu::PresentMode::Fifo;
        }
        config.desired_maximum_frame_latency = 1;
        // A layer-shell/session-lock render target is the complete wallpaper,
        // not a translucent overlay. In particular, module=12 deliberately
        // writes `scale^4` to fragment alpha just as the WebGL shader does;
        // letting the Wayland compositor consume that alpha makes the desktop
        // show through and applies an unintended second visual modulation.
        if capabilities
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::Opaque)
        {
            config.alpha_mode = wgpu::CompositeAlphaMode::Opaque;
        }
        log::debug!(
            target: "minecraft_plus_wayland::surface",
            "wgpu surface configure: size={}x{}, format={:?}, is_srgb={}, color_space={:?}, \
             alpha_mode={:?}, present_mode={:?}, available_formats={:?}",
            config.width,
            config.height,
            config.format,
            config.format.is_srgb(),
            config.color_space,
            config.alpha_mode,
            config.present_mode,
            capabilities.formats,
        );
        self.surface.configure(&self.device, &config);
        self.config = Some(config);
        Ok(())
    }

    pub fn render(
        &mut self,
        module: &mut dyn Module,
        frame: FrameInfo,
        lock_overlay: Option<(&mut TriangleAnimation, LockVisual)>,
    ) -> RendererResult<RenderOutcome> {
        let Some(config) = self.config.as_ref() else {
            return Ok(RenderOutcome::Skipped);
        };

        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => texture,
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => {
                drop(texture);
                self.reconfigure()?;
                return Ok(RenderOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.reconfigure()?;
                return Ok(RenderOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.recreate_surface()?;
                return Ok(RenderOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(RenderOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("wgpu surface acquisition failed validation".into());
            }
        };

        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor {
                format: Some(config.format),
                ..Default::default()
            });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("minecraft-plus-wayland frame"),
            });
        let context = self.context();
        module.render(&context, &mut encoder, &view, frame);
        if let Some((overlay, visual)) = lock_overlay {
            overlay.draw(&context, &mut encoder, &view, visual);
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(surface_texture);

        Ok(RenderOutcome::Presented)
    }

    fn reconfigure(&mut self) -> RendererResult<()> {
        let config = self
            .config
            .clone()
            .ok_or("cannot reconfigure an unconfigured surface")?;
        self.surface.configure(&self.device, &config);
        Ok(())
    }

    fn recreate_surface(&mut self) -> RendererResult<()> {
        self.surface = Self::create_surface(
            &self.instance,
            self.raw_display_handle,
            self.raw_window_handle,
        )?;
        self.reconfigure()
    }

    fn create_surface(
        instance: &wgpu::Instance,
        raw_display_handle: RawDisplayHandle,
        raw_window_handle: RawWindowHandle,
    ) -> Result<wgpu::Surface<'static>, wgpu::CreateSurfaceError> {
        // The App keeps the Wayland surface and Connection alive until Renderer is dropped.
        unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(raw_display_handle),
                raw_window_handle,
            })
        }
    }
}

fn preferred_surface_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    formats
        .iter()
        .copied()
        .find(wgpu::TextureFormat::is_srgb)
        .or_else(|| formats.first().copied())
}

#[cfg(any(debug_assertions, test))]
fn debug_surface_format_override(
    value: &str,
    supported: &[wgpu::TextureFormat],
) -> Result<wgpu::TextureFormat, String> {
    let format = match value.to_ascii_lowercase().as_str() {
        "rgba8unorm" | "rgba8-unorm" => wgpu::TextureFormat::Rgba8Unorm,
        "bgra8unorm" | "bgra8-unorm" => wgpu::TextureFormat::Bgra8Unorm,
        "rgba8unormsrgb" | "rgba8unorm-srgb" | "rgba8-unorm-srgb" => {
            wgpu::TextureFormat::Rgba8UnormSrgb
        }
        "bgra8unormsrgb" | "bgra8unorm-srgb" | "bgra8-unorm-srgb" => {
            wgpu::TextureFormat::Bgra8UnormSrgb
        }
        _ => {
            return Err(format!(
                "unsupported value {value:?}; expected rgba8unorm, bgra8unorm, \
                 rgba8unorm-srgb, or bgra8unorm-srgb"
            ));
        }
    };
    if !supported.contains(&format) {
        return Err(format!(
            "requested format {format:?} is unavailable; compositor supports {supported:?}"
        ));
    }
    Ok(format)
}

#[cfg(test)]
mod tests {
    use super::{debug_surface_format_override, preferred_surface_format};

    #[test]
    fn surface_format_prefers_srgb_over_capability_order() {
        assert_eq!(
            preferred_surface_format(&[
                wgpu::TextureFormat::Bgra8Unorm,
                wgpu::TextureFormat::Rgba8UnormSrgb,
                wgpu::TextureFormat::Bgra8UnormSrgb,
            ]),
            Some(wgpu::TextureFormat::Rgba8UnormSrgb)
        );
    }

    #[test]
    fn surface_format_falls_back_to_first_non_srgb_format() {
        assert_eq!(
            preferred_surface_format(&[
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::TextureFormat::Bgra8Unorm,
            ]),
            Some(wgpu::TextureFormat::Rgba8Unorm)
        );
        assert_eq!(preferred_surface_format(&[]), None);
    }

    #[test]
    fn debug_override_accepts_only_supported_unorm_surface_formats() {
        let supported = [
            wgpu::TextureFormat::Rgba8UnormSrgb,
            wgpu::TextureFormat::Rgba8Unorm,
        ];
        assert_eq!(
            debug_surface_format_override("rgba8unorm", &supported).unwrap(),
            wgpu::TextureFormat::Rgba8Unorm
        );
        assert!(debug_surface_format_override("bgra8unorm", &supported).is_err());
        assert!(debug_surface_format_override("rgba16float", &supported).is_err());
    }
}
