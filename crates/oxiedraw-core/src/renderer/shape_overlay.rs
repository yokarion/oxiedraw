//! Shape-overlay GPU resources: a fullscreen pipeline that rasterises
//! one of the shape primitives directly from push constants and OVER-
//! blends the result into the bound target (preview image during drag,
//! layer image at commit).
//!
//! No textures are uploaded - the entire shape lives in 48 bytes of
//! push data. The only binding is the existing selection mask, used
//! to clip the fill against an active selection.

use ash::{Device, vk};

use super::RendererError;
use super::pass::{
    FullscreenPass, allocate_sampler_set, linear_clamp_sampler, over_blend, pipeline_layout,
    sampler_descriptor_pool, sampler_set_layout,
};

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const SHAPE_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shape.frag.spv"));

/// 48 bytes: 3x vec4 (color, rect, extra). See `shape.frag`.
pub(super) const SHAPE_PUSH_BYTES: u32 = 48;

pub(super) struct ShapeOverlayResources {
    pub sampler: vk::Sampler,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub descriptor_pool: vk::DescriptorPool,
    pub descriptor_set: vk::DescriptorSet,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
}

impl ShapeOverlayResources {
    pub(super) fn new(
        device: &Device,
        canvas_render_pass: vk::RenderPass,
        selection_mask_view: vk::ImageView,
    ) -> Result<Self, RendererError> {
        // Linear so the selection-mask edge is interpolated, matching the
        // brush composite path; the shape itself is rasterised in the
        // fragment shader and doesn't go through this sampler.
        let sampler = linear_clamp_sampler(device)?;
        let descriptor_set_layout = sampler_set_layout(device, 1)?;
        let descriptor_pool = sampler_descriptor_pool(device, 1)?;
        let descriptor_set = allocate_sampler_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            &[selection_mask_view],
            sampler,
        )?;
        let layout = pipeline_layout(device, descriptor_set_layout, SHAPE_PUSH_BYTES)?;
        // Premultiplied OVER - colour is `coverage * push.color`, alpha is
        // `coverage * push.color.a`, both blend identically.
        let pipeline = FullscreenPass {
            vert_spv: COMPOSITE_VERT_SPV,
            frag_spv: SHAPE_FRAG_SPV,
            render_pass: canvas_render_pass,
            layout,
            blend: over_blend(),
        }
        .build(device)?;
        Ok(Self {
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            descriptor_set,
            layout,
            pipeline,
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
        }
    }
}
