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
    /// Get a descriptor of the dmabuf display image (fd + DRM metadata).
    #[must_use]
    pub fn display_descriptor(&self) -> DmabufDescriptor {
        self.display.descriptor()
    }

    /// Copy `source` into the dmabuf display image and fence-wait so the
    /// display server can safely read it.
    pub fn present_to_display(&mut self, source: PresentSource) -> Result<(), RendererError> {
        let src_image = match source {
            PresentSource::Canvas => self.canvas.handle,
            PresentSource::Preview => self.preview.handle,
        };
        let display_old_layout = self.display_old_layout();
        self.record_and_submit(|this| {
            this.record_present_copy(src_image, display_old_layout);
            Ok(())
        })?;
        self.display_initialised = true;
        Ok(())
    }

    /// The display image's layout coming into a present: GENERAL once it
    /// has been written at least once, UNDEFINED on the very first copy.
    pub(super) fn display_old_layout(&self) -> vk::ImageLayout {
        if self.display_initialised {
            vk::ImageLayout::GENERAL
        } else {
            vk::ImageLayout::UNDEFINED
        }
    }

    /// Record the dmabuf-copy commands (display barriers + `src_image` ->
    /// display copy) into the current command buffer. Caller wraps this in
    /// `record_and_submit` and sets `display_initialised` afterward. `src_image`
    /// must be in GENERAL and is restored to GENERAL on return.
    pub(super) fn record_present_copy(
        &self,
        src_image: vk::Image,
        display_old_layout: vk::ImageLayout,
    ) {
        let extent = self.canvas.extent;
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
                    self.display.image,
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
            .src_offset(vk::Offset3D::default())
            .dst_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .dst_offset(vk::Offset3D::default())
            .extent(extent);
        unsafe {
            self.device.cmd_copy_image(
                self.command_buffer,
                src_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.display.image,
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
                    .image(self.display.image)
                    .subresource_range(full_subresource_range())],
            );
        }
    }
}
