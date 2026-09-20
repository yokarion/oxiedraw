//! Downscaled, non-blocking readback of what the canvas shows, for the
//! timelapse recorder. It has its own command buffer and fence, so polling it
//! never waits on the drawing path's ring slots.

use ash::{Device, vk};
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::Allocator;

use super::super::RendererError;
use super::super::resources::{Buffer, Image};
use super::{CANVAS_BYTES_PER_PIXEL, CANVAS_FORMAT, VulkanRenderer, full_image_barrier};

pub(in crate::renderer) struct FrameCapture {
    halvings: u32,
    source: vk::Extent2D,
    /// One image per halving; blitting 2:1 with a linear filter is an exact box average.
    levels: Vec<Image>,
    readback: Buffer,
    extent: vk::Extent2D,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    pending: bool,
}

impl FrameCapture {
    fn new(
        device: &Device,
        allocator: &mut Allocator,
        pool: vk::CommandPool,
        source: vk::Extent2D,
        halvings: u32,
    ) -> Result<Self, RendererError> {
        let mut levels = Vec::new();
        let mut extent = source;
        for _ in 0..halvings {
            extent = vk::Extent2D {
                width: (extent.width / 2).max(1),
                height: (extent.height / 2).max(1),
            };
            levels.push(Image::new_2d(
                device,
                allocator,
                "capture-level",
                CANVAS_FORMAT,
                extent,
                vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST,
                vk::ImageAspectFlags::COLOR,
            )?);
        }
        let readback = Buffer::new(
            device,
            allocator,
            "capture-readback",
            u64::from(extent.width) * u64::from(extent.height) * CANVAS_BYTES_PER_PIXEL,
            vk::BufferUsageFlags::TRANSFER_DST,
            MemoryLocation::GpuToCpu,
        )?;
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cmd = unsafe { device.allocate_command_buffers(&alloc)? }[0];
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        let fence = unsafe { device.create_fence(&fence_info, None)? };
        Ok(Self { halvings, source, levels, readback, extent, cmd, fence, pending: false })
    }

    /// # Safety
    /// The capture must not be in flight.
    pub(in crate::renderer) unsafe fn destroy(
        self,
        device: &Device,
        allocator: &mut Allocator,
        pool: vk::CommandPool,
    ) {
        unsafe {
            device.destroy_fence(self.fence, None);
            device.free_command_buffers(pool, &[self.cmd]);
            self.readback.destroy(device, allocator);
            for level in self.levels {
                level.destroy(device, allocator);
            }
        }
    }

    fn record(&self, device: &Device, src: vk::Image) -> Result<(), RendererError> {
        let cmd = self.cmd;
        let barrier = |image, old, new| unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[full_image_barrier(image, old, new)],
            );
        };
        let layers = vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        };
        let corner = |e: vk::Extent2D| vk::Offset3D {
            x: i32::try_from(e.width).unwrap_or(i32::MAX),
            y: i32::try_from(e.height).unwrap_or(i32::MAX),
            z: 1,
        };

        let begin = vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(cmd, &begin)?;
        }
        barrier(src, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        let (mut prev, mut prev_layout, mut prev_extent) = (src, vk::ImageLayout::GENERAL, self.source);
        for level in &self.levels {
            let extent = vk::Extent2D { width: level.extent.width, height: level.extent.height };
            barrier(level.handle, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
            let blit = vk::ImageBlit::default()
                .src_subresource(layers)
                .src_offsets([vk::Offset3D::default(), corner(prev_extent)])
                .dst_subresource(layers)
                .dst_offsets([vk::Offset3D::default(), corner(extent)]);
            unsafe {
                device.cmd_blit_image(
                    cmd,
                    prev,
                    prev_layout,
                    level.handle,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[blit],
                    vk::Filter::LINEAR,
                );
            }
            barrier(level.handle, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
            (prev, prev_layout, prev_extent) = (level.handle, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, extent);
        }
        let region = vk::BufferImageCopy::default()
            .image_subresource(layers)
            .image_extent(vk::Extent3D { width: self.extent.width, height: self.extent.height, depth: 1 });
        unsafe {
            device.cmd_copy_image_to_buffer(cmd, prev, prev_layout, self.readback.handle, &[region]);
        }
        // Later passes that write the source must wait for these reads.
        barrier(src, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        let host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.readback.handle)
            .size(vk::WHOLE_SIZE);
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                &[host],
                &[],
            );
            device.end_command_buffer(cmd)?;
        }
        Ok(())
    }
}

impl VulkanRenderer {
    /// Queue a halved copy of the composite (or the stroke preview) into host
    /// memory. False while the previous capture is still in flight.
    pub fn begin_capture(&mut self, from_preview: bool, halvings: u32) -> Result<bool, RendererError> {
        let source = self.canvas_extent_2d();
        let reusable = self
            .capture
            .as_ref()
            .is_some_and(|c| c.halvings == halvings && c.source == source);
        if !reusable {
            if let Some(old) = self.capture.take() {
                unsafe {
                    self.device.wait_for_fences(&[old.fence], true, u64::MAX)?;
                    old.destroy(&self.device, &mut self.allocator, self.command_pool);
                }
            }
            let capture = FrameCapture::new(&self.device, &mut self.allocator, self.command_pool, source, halvings)?;
            self.capture = Some(capture);
        }
        let src = if from_preview { self.preview.handle } else { self.canvas.handle };
        let Some(capture) = self.capture.as_mut() else {
            return Ok(false);
        };
        if capture.pending {
            return Ok(false);
        }
        capture.record(&self.device, src)?;
        let cmds = [capture.cmd];
        let submit = vk::SubmitInfo::default().command_buffers(&cmds);
        unsafe {
            self.device.reset_fences(&[capture.fence])?;
            self.device.queue_submit(self.queue, &[submit], capture.fence)?;
        }
        capture.pending = true;
        Ok(true)
    }

    /// A capture was started and hasn't been taken yet.
    #[must_use]
    pub fn capture_pending(&self) -> bool {
        self.capture.as_ref().is_some_and(|c| c.pending)
    }

    /// Copy a finished capture into `out` and return its size. `None` when no
    /// capture is queued, or (unless `wait`) the GPU hasn't finished it yet.
    pub fn poll_capture(&mut self, out: &mut Vec<u8>, wait: bool) -> Result<Option<(u32, u32)>, RendererError> {
        let Some(capture) = self.capture.as_mut() else {
            return Ok(None);
        };
        if !capture.pending {
            return Ok(None);
        }
        let done = if wait {
            unsafe { self.device.wait_for_fences(&[capture.fence], true, u64::MAX)? };
            true
        } else {
            unsafe { self.device.get_fence_status(capture.fence)? }
        };
        if !done {
            return Ok(None);
        }
        capture.pending = false;
        let len = capture.extent.width as usize * capture.extent.height as usize * 4;
        let bytes = capture.readback.mapped().ok_or(RendererError::StagingNotMapped)?;
        out.clear();
        out.extend_from_slice(&bytes[..len]);
        Ok(Some((capture.extent.width, capture.extent.height)))
    }
}

#[cfg(all(test, feature = "gpu-tests"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use oxiedraw_utils::geometry::Size;

    use super::*;

    fn filled(size: Size) -> VulkanRenderer {
        let mut r = VulkanRenderer::new(size).unwrap();
        r.clear_canvas([1.0, 0.0, 0.0, 1.0]).unwrap();
        r
    }

    #[test]
    fn halved_capture_matches_the_canvas_colour() {
        let mut r = filled(Size::new(64, 48));
        assert!(r.begin_capture(false, 2).unwrap());
        let mut out = Vec::new();
        let size = r.poll_capture(&mut out, true).unwrap();
        assert_eq!(size, Some((16, 12)));
        assert_eq!(out.len(), 16 * 12 * 4);
        assert!(out.chunks_exact(4).all(|p| p == [0, 0, 255, 255]), "BGRA red everywhere");
    }

    #[test]
    fn full_size_capture_equals_read_canvas() {
        let mut r = filled(Size::new(33, 17));
        let expected = r.read_canvas().unwrap();
        assert!(r.begin_capture(false, 0).unwrap());
        let mut out = Vec::new();
        assert_eq!(r.poll_capture(&mut out, true).unwrap(), Some((33, 17)));
        assert_eq!(out, expected);
    }

    #[test]
    fn a_second_capture_waits_for_the_first_to_be_taken() {
        let mut r = filled(Size::new(8, 8));
        assert!(r.begin_capture(false, 1).unwrap());
        assert!(!r.begin_capture(false, 1).unwrap());
        let mut out = Vec::new();
        assert!(r.poll_capture(&mut out, true).unwrap().is_some());
        assert!(r.poll_capture(&mut out, false).unwrap().is_none());
        assert!(r.begin_capture(false, 1).unwrap());
    }
}
