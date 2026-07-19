//! Fill-overlay GPU resources: a canvas-sized R8 distance mask plus
//! the fullscreen pipeline that overlays the fill colour onto the
//! preview image, clipped to a sweeping reveal radius.
//!
//! The mask is uploaded **once** at the start of an animation; per
//! frame only a single push float (`reveal`) changes. The fragment
//! shader discards pixels outside the fill region or beyond the
//! current radius, so per-frame cost stays flat regardless of canvas
//! size.

use ash::{Device, vk};
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::pass::{
    FullscreenPass, allocate_sampler_set, nearest_clamp_sampler, over_blend, pipeline_layout,
    sampler_descriptor_pool, sampler_set_layout,
};
use super::resources::Image;

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const FILL_OVERLAY_FRAG_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/fill_overlay.frag.spv"));

/// 16 bytes: vec4 color (premultiplied) + 4 bytes for the reveal float
/// (with alignment-padding handled by GLSL `vec4` rules - see shader).
pub(super) const FILL_OVERLAY_PUSH_BYTES: u32 = 20;

pub(super) struct FillOverlayResources {
    /// Canvas-sized `R8_UNORM` distance mask. Uploaded by
    /// `write_fill_mask`; read by the `fill_overlay` shader.
    pub mask: Image,

    pub sampler: vk::Sampler,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub descriptor_pool: vk::DescriptorPool,
    pub descriptor_set: vk::DescriptorSet,

    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
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
            vk::Format::R8_UNORM,
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
        // Premultiplied OVER - same as the brush/composite pipeline, so
        // the fill colour layers cleanly on top of whatever the preview
        // image already shows for that pixel.
        let pipeline = FullscreenPass {
            vert_spv: COMPOSITE_VERT_SPV,
            frag_spv: FILL_OVERLAY_FRAG_SPV,
            render_pass: canvas_render_pass,
            layout,
            blend: over_blend(),
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
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
            self.mask.destroy(device, allocator);
        }
    }
}
