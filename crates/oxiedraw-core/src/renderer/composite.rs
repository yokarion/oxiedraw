//! Composites the stroke buffer (R8) onto the canvas (sRGB) at a given
//! tint + opacity, plus the eraser variant that punches coverage back out.

use ash::{Device, vk};

use super::RendererError;
use super::pass::{
    FullscreenPass, allocate_sampler_set, dst_out_blend, linear_clamp_sampler, over_blend,
    pipeline_layout, sampler_descriptor_pool, sampler_set_layout,
};

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const COMPOSITE_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.frag.spv"));

/// vec4 tint + one float opacity.
const COMPOSITE_PUSH_BYTES: u32 = 20;

/// Fullscreen triangle, premultiplied-alpha OVER blend.
pub(super) struct CompositePipeline {
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    /// Same shaders/layout as `pipeline` but a DST_OUT blend: it subtracts
    /// the stroke coverage from the target instead of adding tinted color.
    /// Used by the eraser to punch a hole in the target layer.
    pub erase_pipeline: vk::Pipeline,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub descriptor_pool: vk::DescriptorPool,
    pub descriptor_set: vk::DescriptorSet,
    pub sampler: vk::Sampler,
}

impl CompositePipeline {
    pub(super) fn new(
        device: &Device,
        canvas_render_pass: vk::RenderPass,
        stroke_image_view: vk::ImageView,
        selection_image_view: vk::ImageView,
    ) -> Result<Self, RendererError> {
        let descriptor_set_layout = sampler_set_layout(device, 2)?;
        let layout = pipeline_layout(device, descriptor_set_layout, COMPOSITE_PUSH_BYTES)?;

        let mut pass = FullscreenPass {
            vert_spv: COMPOSITE_VERT_SPV,
            frag_spv: COMPOSITE_FRAG_SPV,
            render_pass: canvas_render_pass,
            layout,
            blend: over_blend(),
        };
        let pipeline = pass.build(device)?;
        pass.blend = dst_out_blend();
        let erase_pipeline = pass.build(device)?;

        // Linear filtering on the stroke buffer.
        let sampler = linear_clamp_sampler(device)?;
        let descriptor_pool = sampler_descriptor_pool(device, 2)?;
        let descriptor_set = allocate_sampler_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            &[stroke_image_view, selection_image_view],
            sampler,
        )?;

        Ok(Self {
            layout,
            pipeline,
            erase_pipeline,
            descriptor_set_layout,
            descriptor_pool,
            descriptor_set,
            sampler,
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing this pipeline is in flight.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline(self.erase_pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
        }
    }
}
