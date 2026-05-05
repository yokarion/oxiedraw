//! Scratch resources for the eraser's live preview.
//!
//! Erasing removes coverage from the target layer only, so the preview
//! cannot just composite the stroke over the flattened stack like the
//! brush does. Instead it builds the target layer with the stroke punched
//! out into this canvas-sized scratch image, then composites that scratch
//! back over the layers below. The scratch carries its own layer-composite
//! descriptor set so it can be sampled like any other layer.

use ash::{Device, vk};
use gpu_allocator::vulkan::Allocator;

use super::RendererError;
use super::resources::Image;
use super::vulkan::{create_framebuffer_for_view, create_sampled_image_set};

pub(super) struct ErasePreview {
    pub scratch: Image,
    pub framebuffer: vk::Framebuffer,
    descriptor_pool: vk::DescriptorPool,
    /// Binds `scratch` for the layer-composite pipeline.
    pub composite_set: vk::DescriptorSet,
}

impl ErasePreview {
    pub(super) fn new(
        device: &Device,
        allocator: &mut Allocator,
        canvas_extent: vk::Extent2D,
        canvas_render_pass: vk::RenderPass,
        layer_composite_set_layout: vk::DescriptorSetLayout,
        layer_composite_sampler: vk::Sampler,
    ) -> Result<Self, RendererError> {
        let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED;
        let scratch = Image::new_2d(
            device,
            allocator,
            "erase-scratch",
            super::vulkan::CANVAS_FORMAT,
            canvas_extent,
            usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let framebuffer =
            create_framebuffer_for_view(device, canvas_render_pass, canvas_extent, scratch.view)?;
        let (descriptor_pool, composite_set) = create_sampled_image_set(
            device,
            layer_composite_set_layout,
            layer_composite_sampler,
            scratch.view,
        )?;

        Ok(Self {
            scratch,
            framebuffer,
            descriptor_pool,
            composite_set,
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing these resources is in flight.
    pub(super) unsafe fn destroy(self, device: &Device, allocator: &mut Allocator) {
        unsafe {
            device.destroy_framebuffer(self.framebuffer, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            self.scratch.destroy(device, allocator);
        }
    }
}
