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

/// Selects whether a surface-writing shader must compensate for the target's
/// automatic sRGB encode. Off-screen numeric-domain passes choose their entry
/// explicitly instead of using this helper.
pub(super) fn web_surface_fragment_entry(
    surface_format: wgpu::TextureFormat,
    srgb_entry: &'static str,
    unorm_entry: &'static str,
) -> &'static str {
    let entry = if surface_format.is_srgb() {
        srgb_entry
    } else {
        unorm_entry
    };
    log::debug!(
        target: "minecraft_plus_wayland::surface",
        "Web numeric RGB output: surface_format={surface_format:?}, is_srgb={}, fragment_entry={entry}",
        surface_format.is_srgb(),
    );
    entry
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
