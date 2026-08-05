//! Fill-overlay GPU resources: a canvas-sized R8G8 distance/share mask
//! plus the fullscreen pipelines that hide the not-yet-revealed part of
//! an already-committed fill.
//!
//! The mask is uploaded **once** at the start of an animation; per
//! frame only a single push float (`reveal`) changes. The fragment
//! shader discards pixels outside the fill region or already inside
//! the current radius, so per-frame cost stays flat regardless of
//! canvas size.

use ash::{Device, vk};
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::pass::{
    FullscreenPass, allocate_sampler_set, dst_out_blend, nearest_clamp_sampler, over_blend,
    pipeline_layout, sampler_descriptor_pool, sampler_set_layout,
};
use super::resources::Image;

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const FILL_OVERLAY_FRAG_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/fill_overlay.frag.spv"));

/// 16 bytes: vec4 color (premultiplied) + 4 bytes for the reveal float
/// (with alignment-padding handled by GLSL `vec4` rules - see shader).
pub(super) const FILL_OVERLAY_PUSH_BYTES: u32 = 20;

pub(super) struct FillOverlayResources {
    /// Canvas-sized `R8G8_UNORM` mask: R is the normalised distance
    /// from the seed, G the per-pixel coverage. Uploaded by
    /// `upload_fill_mask`; read by the `fill_overlay` shader.
    pub mask: Image,

    pub sampler: vk::Sampler,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub descriptor_pool: vk::DescriptorPool,
    pub descriptor_set: vk::DescriptorSet,

    pub layout: vk::PipelineLayout,
    /// Paints the seed colour back over the un-revealed pixels - undoes
    /// a fill that replaced the region.
    pub pipeline: vk::Pipeline,
    /// Takes the fill's share back out of the un-revealed pixels -
    /// undoes a fill that went in underneath, leaving what was on top
    /// of it in place.
    pub pipeline_behind: vk::Pipeline,
}

impl FillOverlayResources {
    pub(super) fn new(
        device: &Device,
        allocator: &mut Allocator,
        canvas_extent: vk::Extent2D,
        canvas_render_pass: vk::RenderPass,
    ) -> Result<Self, RendererError> {
        let usage = vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::SAMPLED;
        let mask = Image::new_2d(
            device,
            allocator,
            "fill-overlay-mask",
            vk::Format::R8G8_UNORM,
            canvas_extent,
            usage,
            vk::ImageAspectFlags::COLOR,
        )?;

        let sampler = nearest_clamp_sampler(device)?;
        let descriptor_set_layout = sampler_set_layout(device, 1)?;
        let descriptor_pool = sampler_descriptor_pool(device, 1)?;
        let descriptor_set = allocate_sampler_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            &[mask.view],
            sampler,
        )?;
        let layout = pipeline_layout(device, descriptor_set_layout, FILL_OVERLAY_PUSH_BYTES)?;
        // Premultiplied OVER - the un-revealed pixels get the seed
        // colour painted back on top of the committed fill.
        let pipeline = FullscreenPass {
            vert_spv: COMPOSITE_VERT_SPV,
            frag_spv: FILL_OVERLAY_FRAG_SPV,
            render_pass: canvas_render_pass,
            layout,
            blend: over_blend(),
        }
        .build(device)?;
        let pipeline_behind = FullscreenPass {
            vert_spv: COMPOSITE_VERT_SPV,
            frag_spv: FILL_OVERLAY_FRAG_SPV,
            render_pass: canvas_render_pass,
            layout,
            blend: dst_out_blend(),
        }
        .build(device)?;

        Ok(Self {
            mask,
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            descriptor_set,
            layout,
            pipeline,
            pipeline_behind,
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline(self.pipeline_behind, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
            self.mask.destroy(device, allocator);
        }
    }
}
