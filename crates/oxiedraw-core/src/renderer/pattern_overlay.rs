//! Pattern-tool overlay: a canvas-sized premultiplied BGRA image holding the
//! generated pattern, plus the descriptor set the layer-composite pipeline
//! samples it through.
//!
//! There is no shader of its own - the generator rasterises on the CPU and the
//! result is uploaded - so the only pipeline built here is the alpha-preserving
//! variant of the layer composite, used when the target layer is alpha-locked.
//! Everything is allocated on first use: a document that never reaches for the
//! Pattern tool pays nothing.
//!
//! Not to be confused with [`super::pattern_atlas`], which holds the textured
//! brush's tip images.

use ash::{Device, vk};
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::pass::{FullscreenPass, alpha_lock_blend};
use super::resources::Image;
use super::vulkan::create_sampled_image_set;

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const LAYER_COMPOSITE_FRAG_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/layer_composite.frag.spv"));

pub(super) struct PatternOverlay {
    pub image: Image,
    descriptor_pool: vk::DescriptorPool,
    /// Binds `image` for the layer-composite pipeline.
    pub composite_set: vk::DescriptorSet,
    /// Same shaders and layout as the layer composite, alpha-preserving blend.
    pub pipeline_alpha_lock: vk::Pipeline,
}

impl PatternOverlay {
    /// `set_layout`, `sampler` and `layout` come from the layer-composite
    /// pipeline, so the overlay composites through exactly the path a layer
    /// does.
    pub(super) fn new(
        device: &Device,
        allocator: &mut Allocator,
        canvas_extent: vk::Extent2D,
        canvas_render_pass: vk::RenderPass,
        set_layout: vk::DescriptorSetLayout,
        sampler: vk::Sampler,
        layout: vk::PipelineLayout,
    ) -> Result<Self, RendererError> {
        let image = Image::new_2d(
            device,
            allocator,
            "pattern-overlay",
            super::vulkan::CANVAS_FORMAT,
            canvas_extent,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::COLOR,
        )?;
        let (descriptor_pool, composite_set) =
            create_sampled_image_set(device, set_layout, sampler, image.view)?;
        let pipeline_alpha_lock = FullscreenPass {
            vert_spv: COMPOSITE_VERT_SPV,
            frag_spv: LAYER_COMPOSITE_FRAG_SPV,
            render_pass: canvas_render_pass,
            layout,
            blend: alpha_lock_blend(),
        }
        .build(device)?;

        Ok(Self {
            image,
            descriptor_pool,
            composite_set,
            pipeline_alpha_lock,
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_pipeline(self.pipeline_alpha_lock, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            self.image.destroy(device, allocator);
        }
    }
}
