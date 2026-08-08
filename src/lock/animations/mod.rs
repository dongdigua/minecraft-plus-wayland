use std::{error::Error, time::Instant};

use crate::{
    lock::state::LockVisual,
    modules::{RenderContext, RenderSize},
};

mod creeper;
mod torch;

use creeper::CreeperAnimation;
use torch::TorchAnimation;

/// Per-output router for lock-only visuals. Layer-shell render targets never construct this type.
pub struct LockAnimation {
    creeper: CreeperAnimation,
    torch: TorchAnimation,
}

impl LockAnimation {
    pub fn new() -> Self {
        Self {
            creeper: CreeperAnimation::new(),
            torch: TorchAnimation::new(),
        }
    }

    pub fn ensure_initialized(
        &mut self,
        context: &RenderContext<'_>,
        size: RenderSize,
    ) -> Result<(), Box<dyn Error>> {
        self.creeper.ensure_initialized(context)?;
        self.torch.ensure_initialized(context, size)
    }

    pub fn wants_continuous_frames(&self, visual: LockVisual, frame_time: Instant) -> bool {
        match visual {
            LockVisual::Torch { .. } => self.torch.wants_continuous_frames(),
            _ => visual.wants_continuous_frames(frame_time),
        }
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
        match visual {
            LockVisual::Torch { mask, .. } => self.torch.draw(context, encoder, target, size, mask),
            LockVisual::Hidden => {}
            _ => self
                .creeper
                .draw(context, encoder, target, size, visual, frame_time),
        }
    }
}

impl Default for LockAnimation {
    fn default() -> Self {
        Self::new()
    }
}
