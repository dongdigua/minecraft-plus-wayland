use std::error::Error;

use crate::{
    lock::state::LockVisual,
    modules::{RenderContext, RenderSize},
};

mod torch;
mod triangle;

use torch::TorchAnimation;
use triangle::TriangleAnimation;

/// Per-output router for lock-only visuals. Layer-shell render targets never construct this type.
pub struct LockAnimation {
    triangle: TriangleAnimation,
    torch: TorchAnimation,
}

impl LockAnimation {
    pub fn new() -> Self {
        Self {
            triangle: TriangleAnimation::new(),
            torch: TorchAnimation::new(),
        }
    }

    pub fn ensure_initialized(
        &mut self,
        context: &RenderContext<'_>,
        size: RenderSize,
    ) -> Result<(), Box<dyn Error>> {
        self.triangle.ensure_initialized(context);
        self.torch.ensure_initialized(context, size)
    }

    pub fn wants_continuous_frames(&self, visual: LockVisual) -> bool {
        match visual {
            LockVisual::Torch { state_id, .. } => self.torch.wants_continuous_frames(state_id),
            LockVisual::AuthenticatedGreen { .. } => true,
            _ => false,
        }
    }

    pub fn draw(
        &mut self,
        context: &RenderContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        size: RenderSize,
        visual: LockVisual,
    ) {
        match visual {
            LockVisual::Torch { mask, state_id } => self
                .torch
                .draw(context, encoder, target, size, mask, state_id),
            LockVisual::Hidden => {}
            _ => self.triangle.draw(context, encoder, target, visual),
        }
    }
}

impl Default for LockAnimation {
    fn default() -> Self {
        Self::new()
    }
}
