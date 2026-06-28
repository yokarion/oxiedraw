//! Zero-copy dmabuf present path.
//!
//! `present_to_display` copies the canvas (or preview) into the
//! exportable dmabuf image. GTK reads the same memory via the imported
//! dmabuf fd, no `PCIe` round-trip.

use ash::vk;

use super::super::RendererError;
use super::super::dmabuf::DmabufDescriptor;
use super::{PresentSource, VulkanRenderer, full_image_barrier, full_subresource_range};

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
        let src_image = match source {
            PresentSource::Canvas => self.canvas.handle,
            PresentSource::Preview => self.preview.handle,
        };
        // Async: don't stall the input loop on the dmabuf copy. GTK syncs to our
        // GPU writes via dma-buf implicit sync, and same-queue order keeps the
        // next preview write after this copy.
        self.record_and_submit_async(|this| {
            this.record_present_copy(src_image);
            Ok(())
        })?;
        Ok(())
    }

    /// Record the dmabuf-copy commands (display barriers + `src_image` ->
    /// display copy) into the current command buffer. Caller wraps this in
    /// `record_and_submit`. `src_image` must be in GENERAL and is restored to
    /// GENERAL on return.
    ///
    /// Advances `display_cursor` to a fresh buffer first, so GTK never samples
    /// the buffer we write here. Because each buffer is independent, the copy is
    /// always the *full* canvas (a clipped copy would leave the buffer's other
    /// regions holding pixels from however many frames ago it was last written).
    pub(super) fn record_present_copy(&mut self, src_image: vk::Image) {
        self.display_cursor = (self.display_cursor + 1) % self.display.len();
        let dst_image = self.display[self.display_cursor].image;
        // Full-frame copy; old contents are discarded (UNDEFINED), not preserved.
        let display_old_layout = vk::ImageLayout::UNDEFINED;
        let (clip_offset, clip_extent) =
            (vk::Offset3D::default(), self.canvas.extent);
        // No queue-family transfer to FOREIGN_EXT - we rely on implicit
        // dma-buf sync (kernel propagates the GPU fence).
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[full_image_barrier(
                    dst_image,
                    display_old_layout,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                )],
            );
        }
        self.barrier(
            src_image,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        );

        let copy = vk::ImageCopy::default()
            .src_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_offset(clip_offset)
            .dst_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .dst_offset(clip_offset)
            .extent(clip_extent);
        unsafe {
            self.device.cmd_copy_image(
                self.command_buffer,
                src_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy],
            );
        }

        // Restore source layout, park display in GENERAL with MEMORY_READ
        // so the dma-buf importer sees a coherent view.
        self.barrier(
            src_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::ImageLayout::GENERAL,
        );
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::MEMORY_READ)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(dst_image)
                    .subresource_range(full_subresource_range())],
            );
        }
    }
}
