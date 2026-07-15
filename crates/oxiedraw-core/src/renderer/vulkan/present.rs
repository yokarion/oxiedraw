//! Zero-copy dmabuf present path.
//!
//! `present_to_display` copies the canvas (or preview) into the
//! exportable dmabuf image. GTK reads the same memory via the imported
//! dmabuf fd, no `PCIe` round-trip.

use ash::vk;

use super::super::RendererError;
use super::super::dmabuf::DmabufDescriptor;
use super::{PresentSource, VulkanRenderer};

impl VulkanRenderer {
    /// Get a descriptor (fd + DRM metadata) of the display buffer most recently
    /// written by [`Self::record_present_copy`] - the one GTK should import now.
    #[must_use]
    pub fn display_descriptor(&self) -> DmabufDescriptor {
        self.display[self.display_cursor].descriptor()
    }

    /// Copy `source` into the next display buffer and fence-wait so the
    /// display server can safely read it.
    pub fn present_to_display(&mut self, source: PresentSource) -> Result<(), RendererError> {
        let (src_image, src_view) = match source {
            PresentSource::Canvas => (self.canvas.handle, self.canvas.view),
            PresentSource::Preview => (self.preview.handle, self.preview.view),
        };
        // Async: don't stall the input loop on the dmabuf present. GTK syncs to
        // our GPU writes via dma-buf implicit sync, and same-queue order keeps
        // the next preview write after this pass.
        self.record_and_submit_async(|this| {
            this.record_present_copy(src_image, src_view);
            Ok(())
        })?;
        Ok(())
    }

    /// Record the present into the current command buffer: a full-frame pass
    /// that converts `src_image` (premultiplied-linear, sampled via `src_view`)
    /// to the premultiplied-gamma display buffer. Caller wraps this in
    /// `record_and_submit`. `src_image` must be in GENERAL.
    ///
    /// Advances `display_cursor` to a fresh buffer first, so GTK never samples
    /// the buffer we write here. The pass always covers the *full* canvas (the
    /// render pass discards prior contents), independent of any active clip.
    pub(super) fn record_present_copy(&mut self, src_image: vk::Image, src_view: vk::ImageView) {
        self.display_cursor = (self.display_cursor + 1) % self.display.len();
        let framebuffer = self.display_framebuffers[self.display_cursor];
        let slot = self.current_ring_slot();

        // Point this ring slot's source set at the image we're converting. The
        // slot's previous submission is already fenced (submit_to_ring waits
        // before recording), so the set is free to rewrite.
        self.present_convert
            .bind_source(&self.device, slot, src_view);
        // Make the source's prior writes (composite passes in this same submit)
        // visible to the fragment sampler.
        self.barrier(src_image, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);

        let extent = self.canvas_extent_2d();
        #[allow(clippy::cast_precision_loss)]
        let viewport = vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let full = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        };
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.present_convert.render_pass)
            .framebuffer(framebuffer)
            .render_area(full);
        let set = self.present_convert.src_sets[slot];
        unsafe {
            self.device.cmd_begin_render_pass(
                self.command_buffer,
                &begin,
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_bind_pipeline(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.present_convert.pipeline,
            );
            self.device
                .cmd_set_viewport(self.command_buffer, 0, &[viewport]);
            self.device.cmd_set_scissor(self.command_buffer, 0, &[full]);
            self.device.cmd_bind_descriptor_sets(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.present_convert.layout,
                0,
                &[set],
                &[],
            );
            self.device.cmd_draw(self.command_buffer, 3, 1, 0, 0);
            self.device.cmd_end_render_pass(self.command_buffer);
        }
        // The render pass's subpass dependency flushes the colour writes to
        // MEMORY_READ and leaves the image in GENERAL for the dma-buf importer;
        // implicit dma-buf sync propagates our GPU fence to the compositor.
    }
}
