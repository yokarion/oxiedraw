//! Host-visible I/O for the canvas, layers, stroke buffer and preview.

use ash::vk;

use super::super::RendererError;
use super::{CANVAS_BYTES_PER_PIXEL, VulkanRenderer};

impl VulkanRenderer {
    /// Clear the canvas to `color` (linear RGBA, sRGB-encoded by hardware
    /// on write). Submits and waits.
    pub fn clear_canvas(&mut self, color: [f32; 4]) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            this.cmd_clear_image(this.canvas.handle, color);
            Ok(())
        })
    }

    /// Convenience: `clear_canvas` then `read_canvas`. Smoke tests.
    pub fn clear_and_read(&mut self, color: [f32; 4]) -> Result<Vec<u8>, RendererError> {
        self.clear_canvas(color)?;
        self.read_canvas()
    }

    /// Copy the canvas into the host-visible staging buffer and return
    /// a fresh `Vec<u8>` of the pixels (BGRA8, row-major, no padding).
    pub fn read_canvas(&mut self) -> Result<Vec<u8>, RendererError> {
        let image = self.canvas.handle;
        let extent = self.canvas.extent;
        self.read_image_to_staging(image, extent)?;
        self.copy_staging_bytes()
    }

    /// Read back the display dmabuf that `present_to_display` last wrote, as
    /// BGRA8 (row-major, no padding). Unlike [`Self::read_canvas`] these pixels
    /// are premultiplied *gamma* (`srgb(colour) * alpha`) - the form GTK's
    /// sRGB-space compositing expects. Test/diagnostic helper: the live path
    /// hands this buffer to GTK instead of reading it back.
    pub fn read_display(&mut self) -> Result<Vec<u8>, RendererError> {
        // `present_to_display` submits without waiting; make sure that pass has
        // landed before we copy the buffer out.
        self.wait_last()?;
        let image = self.display[self.display_cursor].image;
        let extent = self.canvas.extent;
        self.read_image_to_staging(image, extent)?;
        self.copy_staging_bytes()
    }

    /// Read back the layer at `idx` into a fresh `Vec<u8>` of BGRA8
    /// pixels (row-major, no padding). Uses the shared staging buffer.
    pub fn read_layer(&mut self, idx: usize) -> Result<Vec<u8>, RendererError> {
        if idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        let image = self.layer_stack.slots[idx].image.handle;
        let extent = self.canvas.extent;
        self.read_image_to_staging(image, extent)?;
        self.copy_staging_bytes()
    }

    /// Like [`Self::read_canvas`] but fills a caller-owned buffer instead
    /// of allocating. Lets repeated readback paths (thumbnails) keep one
    /// buffer alive and avoid a fresh full-canvas allocation per call.
    pub fn read_canvas_into(&mut self, out: &mut Vec<u8>) -> Result<(), RendererError> {
        let image = self.canvas.handle;
        let extent = self.canvas.extent;
        self.read_image_to_staging(image, extent)?;
        self.copy_staging_bytes_into(out)
    }

    /// Like [`Self::read_layer`] but fills a caller-owned buffer.
    pub fn read_layer_into(&mut self, idx: usize, out: &mut Vec<u8>) -> Result<(), RendererError> {
        if idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        let image = self.layer_stack.slots[idx].image.handle;
        let extent = self.canvas.extent;
        self.read_image_to_staging(image, extent)?;
        self.copy_staging_bytes_into(out)
    }

    /// Read back a `w x h` sub-rectangle of layer `idx` at `(x, y)` into
    /// `out` (BGRA8, row-major, tightly packed - `w * h * 4` bytes). Far
    /// cheaper than a full-layer read when only a small dirty region
    /// (e.g. a brush dab's AABB) is needed.
    pub fn read_layer_region_into(
        &mut self,
        idx: usize,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), RendererError> {
        if idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        if w == 0 || h == 0 {
            out.clear();
            return Ok(());
        }
        let image = self.layer_stack.slots[idx].image.handle;
        self.read_image_region_to_staging(image, x, y, w, h)?;
        let len = (w as usize) * (h as usize) * 4;
        let bytes = self
            .staging
            .mapped()
            .ok_or(RendererError::StagingNotMapped)?;
        out.clear();
        out.extend_from_slice(&bytes[..len]);
        Ok(())
    }

    /// Read back a `w x h` sub-rectangle of the composited canvas image at
    /// `(x, y)` into `out` (BGRA8, row-major, tightly packed). Used by the
    /// color picker to sample the visible color under the cursor without a
    /// full-canvas readback.
    pub fn read_canvas_region_into(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), RendererError> {
        if w == 0 || h == 0 {
            out.clear();
            return Ok(());
        }
        let image = self.canvas.handle;
        self.read_image_region_to_staging(image, x, y, w, h)?;
        let len = (w as usize) * (h as usize) * 4;
        let bytes = self
            .staging
            .mapped()
            .ok_or(RendererError::StagingNotMapped)?;
        out.clear();
        out.extend_from_slice(&bytes[..len]);
        Ok(())
    }

    /// Copy a `w x h` sub-rectangle of `image` at `(x, y)` into the
    /// host-visible staging buffer (tightly packed) in one submit.
    pub(super) fn read_image_region_to_staging(
        &mut self,
        image: vk::Image,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            this.barrier(
                image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            );
            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D {
                    x: i32::try_from(x).unwrap_or(0),
                    y: i32::try_from(y).unwrap_or(0),
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                });
            unsafe {
                this.device.cmd_copy_image_to_buffer(
                    this.command_buffer,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    this.staging.handle,
                    &[region],
                );
            }
            this.barrier(
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );
            Ok(())
        })
    }

    /// Upload BGRA8 `pixels` (row-major, no padding) into the GPU layer at `idx`.
    pub fn write_layer(&mut self, idx: usize, pixels: &[u8]) -> Result<(), RendererError> {
        if idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        {
            let staging = self
                .staging
                .mapped_mut()
                .ok_or(RendererError::StagingNotMapped)?;
            let copy_len = pixels.len().min(staging.len());
            staging[..copy_len].copy_from_slice(&pixels[..copy_len]);
        }
        let image = self.layer_stack.slots[idx].image.handle;
        let extent = self.canvas.extent;
        self.write_staging_to_image(image, extent)?;
        self.layer_stack.touch(idx);
        Ok(())
    }

    /// Upload only a sub-rectangle of a layer. `pixels` is tightly packed BGRA8
    /// for the `w x h` region and lands at `(x, y)` in the layer image. The
    /// region is clamped to the canvas. Far cheaper than [`Self::write_layer`] for a
    /// small dirty rect (e.g. a text box on a large canvas): the staging copy
    /// and GPU upload scale with the region, not the whole canvas.
    pub fn write_layer_region(
        &mut self,
        idx: usize,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        pixels: &[u8],
    ) -> Result<(), RendererError> {
        if idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        let cw = self.canvas.extent.width as i32;
        let ch = self.canvas.extent.height as i32;
        // Clamp the destination rect to the canvas; derive the matching source
        // offset within `pixels`.
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w as i32).min(cw).max(x0);
        let y1 = (y + h as i32).min(ch).max(y0);
        let copy_w = (x1 - x0) as u32;
        let copy_h = (y1 - y0) as u32;
        if copy_w == 0 || copy_h == 0 {
            return Ok(());
        }
        {
            let staging = self
                .staging
                .mapped_mut()
                .ok_or(RendererError::StagingNotMapped)?;
            let bpp = CANVAS_BYTES_PER_PIXEL as usize;
            let src_stride = (w as usize) * bpp;
            let dst_stride = (copy_w as usize) * bpp;
            let col_off = ((x0 - x) as usize) * bpp;
            let row_off = (y0 - y) as usize;
            // Region always fits the canvas-sized staging buffer.
            for row in 0..copy_h as usize {
                let src = (row_off + row) * src_stride + col_off;
                let dst = row * dst_stride;
                staging[dst..dst + dst_stride].copy_from_slice(&pixels[src..src + dst_stride]);
            }
        }
        let image = self.layer_stack.slots[idx].image.handle;
        self.write_staging_region_to_image(image, x0, y0, copy_w, copy_h)?;
        self.layer_stack.touch(idx);
        Ok(())
    }

    /// Copy the first `w*h` tightly-packed pixels of staging into `image` at
    /// `(x, y)`. Counterpart of [`write_staging_to_image`] for a sub-rect.
    fn write_staging_region_to_image(
        &mut self,
        image: vk::Image,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            this.barrier(
                image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D { x, y, z: 0 })
                .image_extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                });
            unsafe {
                this.device.cmd_copy_buffer_to_image(
                    this.command_buffer,
                    this.staging.handle,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
            }
            this.barrier(
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );
            Ok(())
        })
    }

    pub(super) fn write_staging_to_image(
        &mut self,
        image: vk::Image,
        extent: vk::Extent3D,
    ) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            this.barrier(
                image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D::default())
                .image_extent(extent);
            unsafe {
                this.device.cmd_copy_buffer_to_image(
                    this.command_buffer,
                    this.staging.handle,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
            }
            this.barrier(
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );
            Ok(())
        })
    }

    /// Read the preview image (call `render_preview` first to populate it).
    pub fn read_preview(&mut self) -> Result<Vec<u8>, RendererError> {
        let image = self.preview.handle;
        let extent = self.preview.extent;
        self.read_image_to_staging(image, extent)?;
        self.copy_staging_bytes()
    }

    pub(super) fn copy_staging_bytes(&self) -> Result<Vec<u8>, RendererError> {
        let len = usize::try_from(self.canvas_size.area() * CANVAS_BYTES_PER_PIXEL)
            .expect("staging size fits in usize");
        let bytes = self
            .staging
            .mapped()
            .ok_or(RendererError::StagingNotMapped)?;
        Ok(bytes[..len].to_vec())
    }

    /// Copy the staging bytes into `out`, reusing its existing capacity.
    pub(super) fn copy_staging_bytes_into(&self, out: &mut Vec<u8>) -> Result<(), RendererError> {
        let len = usize::try_from(self.canvas_size.area() * CANVAS_BYTES_PER_PIXEL)
            .expect("staging size fits in usize");
        let bytes = self
            .staging
            .mapped()
            .ok_or(RendererError::StagingNotMapped)?;
        out.clear();
        out.extend_from_slice(&bytes[..len]);
        Ok(())
    }

    /// Copy the stroke buffer (R8) into a fresh `Vec<u8>` of the R
    /// channel, row-major, no padding. Test / debug helper.
    pub fn read_stroke(&mut self) -> Result<Vec<u8>, RendererError> {
        let extent = self.stroke.extent;
        self.read_image_to_staging(self.stroke.handle, extent)?;
        let len = usize::try_from(self.canvas_size.area()).expect("stroke size fits in usize");
        let bytes = self
            .staging
            .mapped()
            .ok_or(RendererError::StagingNotMapped)?;
        Ok(bytes[..len].to_vec())
    }

    /// Copy `image` into the host-visible staging buffer in one submit.
    pub(super) fn read_image_to_staging(
        &mut self,
        image: vk::Image,
        extent: vk::Extent3D,
    ) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            this.barrier(
                image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            );
            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D::default())
                .image_extent(extent);
            unsafe {
                this.device.cmd_copy_image_to_buffer(
                    this.command_buffer,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    this.staging.handle,
                    &[region],
                );
            }
            this.barrier(
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );
            Ok(())
        })
    }
}
