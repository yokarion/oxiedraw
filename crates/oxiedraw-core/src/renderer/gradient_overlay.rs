//! Gradient-overlay GPU resources: a fullscreen pipeline that samples a
//! baked colour LUT along a Linear/Radial/Square ramp and OVER-blends the
//! result into the bound target (preview image during drag, layer image at
//! commit).
//!
//! Two bindings: the existing selection mask (to clip against an active
//! selection) and a 256x1 RGBA16F LUT owned here. The ramp geometry lives
//! in 32 bytes of push data; only the LUT is uploaded (once per drag).

use ash::{Device, vk};
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::pass::{
    FullscreenPass, allocate_sampler_set, linear_clamp_sampler, over_blend, pipeline_layout,
    sampler_descriptor_pool, sampler_set_layout,
};
use super::resources::Image;

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const GRADIENT_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gradient.frag.spv"));

/// 32 bytes: 2x vec4 (endpoints, extra). See `gradient.frag`.
pub(super) const GRADIENT_PUSH_BYTES: u32 = 32;

/// Width of the LUT image (one texel per 8-bit level).
pub(super) const LUT_WIDTH: u32 = 256;

pub(super) struct GradientOverlayResources {
    /// 256x1 `R16G16B16A16_SFLOAT` LUT, premultiplied linear RGBA. Uploaded
    /// by `set_gradient_lut`; read by the `gradient` shader at binding 1.
    pub lut: Image,

    pub sampler: vk::Sampler,
    pub descriptor_set_layout: vk::DescriptorSetLayout,
    pub descriptor_pool: vk::DescriptorPool,
    pub descriptor_set: vk::DescriptorSet,
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
}

impl GradientOverlayResources {
    pub(super) fn new(
        device: &Device,
        allocator: &mut Allocator,
        canvas_render_pass: vk::RenderPass,
        selection_mask_view: vk::ImageView,
    ) -> Result<Self, RendererError> {
        let lut = Image::new_2d(
            device,
            allocator,
            "gradient-lut",
            vk::Format::R16G16B16A16_SFLOAT,
            vk::Extent2D { width: LUT_WIDTH, height: 1 },
            vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::COLOR,
        )?;

        // Linear so the LUT interpolates smoothly between its 256 texels and
        // the selection-mask edge is interpolated (matching shape/fill).
        let sampler = linear_clamp_sampler(device)?;
        let descriptor_set_layout = sampler_set_layout(device, 2)?;
        let descriptor_pool = sampler_descriptor_pool(device, 2)?;
        let descriptor_set = allocate_sampler_set(
            device,
            descriptor_pool,
            descriptor_set_layout,
            &[selection_mask_view, lut.view],
            sampler,
        )?;
        let layout = pipeline_layout(device, descriptor_set_layout, GRADIENT_PUSH_BYTES)?;
        // Premultiplied OVER - identical to the shape/fill overlays.
        let pipeline = FullscreenPass {
            vert_spv: COMPOSITE_VERT_SPV,
            frag_spv: GRADIENT_FRAG_SPV,
            render_pass: canvas_render_pass,
            layout,
            blend: over_blend(),
        }
        .build(device)?;
        Ok(Self {
            lut,
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
            self.lut.destroy(device, allocator);
        }
    }
}
