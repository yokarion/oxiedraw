use ash::{Device, vk};

use super::RendererError;

/// A render-pass + framebuffer pair targeting a single image. We use
/// `vk::ImageLayout::GENERAL` everywhere so we don't have to track
/// layout transitions across submit boundaries; the renderer's resting
/// layout is GENERAL too.
pub(super) struct ImageTarget {
    pub render_pass: vk::RenderPass,
    pub framebuffer: vk::Framebuffer,
    #[allow(dead_code)]
    pub extent: vk::Extent2D,
}

impl ImageTarget {
    pub(super) fn new(
        device: &Device,
        format: vk::Format,
        extent: vk::Extent2D,
        view: vk::ImageView,
    ) -> Result<Self, RendererError> {
        let attachments = [vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::GENERAL)
            .final_layout(vk::ImageLayout::GENERAL)];
        let color_refs = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::GENERAL)];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_refs)];
        let info = vk::RenderPassCreateInfo::default()
            .attachments(&attachments)
            .subpasses(&subpasses);
        let render_pass = unsafe { device.create_render_pass(&info, None)? };

        let views = [view];
        let fb_info = vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(&views)
            .width(extent.width)
            .height(extent.height)
            .layers(1);
        let framebuffer = unsafe { device.create_framebuffer(&fb_info, None)? };

        Ok(Self {
            render_pass,
            framebuffer,
            extent,
        })
    }

    /// # Safety
    /// Caller must ensure no GPU work referencing this target is in flight.
    pub(super) unsafe fn destroy(self, device: &Device) {
        unsafe {
            device.destroy_framebuffer(self.framebuffer, None);
            device.destroy_render_pass(self.render_pass, None);
        }
    }
}
