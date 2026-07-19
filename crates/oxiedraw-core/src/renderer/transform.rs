//! GPU pipeline for the transform tool's apply step.
//!
//! Samples a source texture with a CPU-precomputed 2x3 inverse affine and
//! writes into a framebuffer (typically the active layer's image for the
//! canvas-clipped pass, plus an arbitrarily-sized AABB image for the
//! extension/off-canvas pass).
//!
//! Render-pass-compatible with the canvas render pass - any framebuffer
//! built around a `CANVAS_FORMAT` colour attachment can be the target.

use ash::{Device, vk};

use super::RendererError;
use super::pass::{FullscreenPass, linear_clamp_sampler, pipeline_layout, replace_blend, sampler_set_layout};

const VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/transform.frag.spv"));

/// Push constant block size in bytes: two `vec4`s (matrix rows packed with
/// the translation in `.z` and a `.w` padding for std140 alignment).
pub(super) const PUSH_BYTES: u32 = 32;

pub(super) struct TransformPipeline {
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub sampler: vk::Sampler,
}

impl TransformPipeline {
    pub(super) fn new(
        device: &Device,
        canvas_render_pass: vk::RenderPass,
    ) -> Result<Self, RendererError> {
        let descriptor_set_layout = sampler_set_layout(device, 1)?;
        let layout = pipeline_layout(device, descriptor_set_layout, PUSH_BYTES)?;
        // We REPLACE the framebuffer contents (it was cleared by the render
        // pass load op), so blending is disabled.
        let pipeline = FullscreenPass {
            vert_spv: VERT_SPV,
            frag_spv: FRAG_SPV,
            render_pass: canvas_render_pass,
            layout,
            blend: replace_blend(),
        }
        .build(device)?;
        // Linear filtering matches the CPU `sample_bilinear` path, and
        // clamp-to-edge matches its `clamp(0, w-1)` behaviour.
        let sampler = linear_clamp_sampler(device)?;
        Ok(Self {
            layout,
            pipeline,
            descriptor_set_layout,
            sampler,
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing this pipeline is in flight.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
        }
    }
}
