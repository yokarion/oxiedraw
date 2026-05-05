//! Public Vulkan operations on the selection mask: clear, fill, invert,
//! shape upload+blend, edge detection, readback.

use ash::vk;

use super::super::RendererError;
use super::super::selection::SelectionBlendMode;
use super::{EdgesBuffer, VulkanRenderer};

impl VulkanRenderer {
    /// Whether the renderer currently treats the mask as live (composite
    /// will clip strokes to it). Toggled by the public selection ops.
    #[must_use]
    pub const fn selection_active(&self) -> bool {
        self.selection_active
    }

    /// Mark the mask inert (composite ignores it). The mask image's
    /// contents are left as-is - they're don't-care while `selection_active`
    /// is false.
    pub const fn deselect(&mut self) {
        self.selection_active = false;
    }

    /// Fill the mask to fully-selected. Logical equivalent of "Select All".
    pub fn select_all(&mut self) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            this.cmd_clear_image(this.selection.mask.handle, [1.0, 0.0, 0.0, 0.0]);
            Ok(())
        })?;
        self.selection_active = true;
        Ok(())
    }

    /// Invert the mask: copy mask -> scratch, clear mask to 1.0, then
    /// blend scratch into mask with the Subtract mode
    /// (`dst*(1-src) = 1 - src`). Equivalent to `mask = 1 - mask`.
    pub fn invert_selection(&mut self) -> Result<(), RendererError> {
        if !self.selection_active {
            // No selection means the mask is don't-care - inverting it
            // would just leave us in the same indeterminate state. Make
            // it explicit: invert of no-selection is no-selection.
            tracing::warn!("invert_selection with no active selection - ignored");
            return Ok(());
        }
        self.copy_mask_to_scratch()?;
        self.record_and_submit(|this| {
            this.cmd_clear_image(this.selection.mask.handle, [1.0, 0.0, 0.0, 0.0]);
            Ok(())
        })?;
        self.run_selection_blend(SelectionBlendMode::Subtract)
    }

    /// Upload a CPU-rasterised shape buffer (R8, canvas-sized, row-major)
    /// to the scratch image, then blend it into the mask with `mode`.
    /// If no selection is currently active, the first `Add` / `Intersect`
    /// / `Replace` becomes a `Replace`; `Subtract` is a no-op.
    pub fn apply_selection_shape(
        &mut self,
        shape_pixels: &[u8],
        mode: SelectionBlendMode,
    ) -> Result<(), RendererError> {
        let extent = self.canvas.extent;
        let expected_bytes = usize::try_from(self.canvas_size.area()).unwrap_or(0);
        if shape_pixels.len() < expected_bytes {
            return Err(RendererError::StagingNotMapped);
        }

        // Promote to "first selection" if needed.
        let effective_mode = if self.selection_active {
            mode
        } else {
            match mode {
                SelectionBlendMode::Subtract => {
                    // Nothing to subtract from.
                    return Ok(());
                }
                _ => SelectionBlendMode::Replace,
            }
        };

        // Stage the R8 bytes.
        {
            let staging = self
                .staging
                .mapped_mut()
                .ok_or(RendererError::StagingNotMapped)?;
            staging[..expected_bytes].copy_from_slice(&shape_pixels[..expected_bytes]);
        }
        // Upload staging -> scratch image (R8, canvas-sized).
        let scratch = self.selection.scratch.handle;
        self.write_staging_to_r8_image(scratch, extent)?;

        self.run_selection_blend(effective_mode)?;
        self.selection_active = true;
        Ok(())
    }

    /// Run the full-res blend pipeline `scratch -> mask` with `mode`'s
    /// blend state. Caller is responsible for having uploaded the desired
    /// source pixels into scratch first.
    fn run_selection_blend(&mut self, mode: SelectionBlendMode) -> Result<(), RendererError> {
        let render_pass = self.selection.mask_target.render_pass;
        let framebuffer = self.selection.mask_target.framebuffer;
        let pipeline = self.selection.blend_pipeline(mode);
        let layout = self.selection.blend_layout;
        let descriptor_set = self.selection.scratch_descriptor_set;
        self.record_and_submit(|this| {
            this.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
            unsafe {
                this.device.cmd_bind_descriptor_sets(
                    this.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &[descriptor_set],
                    &[],
                );
            }
            this.cmd_end_fullscreen_pass();
            Ok(())
        })
    }

    /// Copy the full-res mask image into the scratch image. Both are
    /// canvas-sized R8 in GENERAL layout.
    fn copy_mask_to_scratch(&mut self) -> Result<(), RendererError> {
        let extent = self.canvas.extent;
        let src = self.selection.mask.handle;
        let dst = self.selection.scratch.handle;
        self.record_and_submit(|this| {
            this.barrier(
                src,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            );
            this.barrier(
                dst,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            let region = vk::ImageCopy::default()
                .src_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .dst_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .extent(extent);
            unsafe {
                this.device.cmd_copy_image(
                    this.command_buffer,
                    src,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
            }
            this.barrier(
                src,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );
            this.barrier(
                dst,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );
            Ok(())
        })
    }

    /// Run the edges/downsample pass and read back the small edges buffer.
    pub fn compute_selection_edges(&mut self) -> Result<EdgesBuffer, RendererError> {
        let edges_extent = self.selection.edges_extent;
        // Push constant: 1/canvas_w, 1/canvas_h (single full-res pixel step in uv).
        #[allow(clippy::cast_precision_loss)]
        let inv_size = [
            1.0_f32 / self.canvas.extent.width as f32,
            1.0_f32 / self.canvas.extent.height as f32,
        ];
        let render_pass = self.selection.edges_target.render_pass;
        let framebuffer = self.selection.edges_target.framebuffer;
        let pipeline = self.selection.edges_pipeline;
        let layout = self.selection.edges_layout;
        let descriptor_set = self.selection.mask_descriptor_set;

        self.record_and_submit(|this| {
            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                #[allow(clippy::cast_precision_loss)]
                width: edges_extent.width as f32,
                #[allow(clippy::cast_precision_loss)]
                height: edges_extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            let scissor = vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: edges_extent,
            };
            let begin = vk::RenderPassBeginInfo::default()
                .render_pass(render_pass)
                .framebuffer(framebuffer)
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: edges_extent,
                });
            unsafe {
                this.device.cmd_begin_render_pass(
                    this.command_buffer,
                    &begin,
                    vk::SubpassContents::INLINE,
                );
                this.device.cmd_bind_pipeline(
                    this.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    pipeline,
                );
                this.device
                    .cmd_set_viewport(this.command_buffer, 0, &[viewport]);
                this.device
                    .cmd_set_scissor(this.command_buffer, 0, &[scissor]);
                this.device.cmd_bind_descriptor_sets(
                    this.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &[descriptor_set],
                    &[],
                );
                let push_bytes = std::slice::from_raw_parts(
                    inv_size.as_ptr().cast::<u8>(),
                    std::mem::size_of_val(&inv_size),
                );
                this.device.cmd_push_constants(
                    this.command_buffer,
                    layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    push_bytes,
                );
                this.device.cmd_draw(this.command_buffer, 3, 1, 0, 0);
                this.device.cmd_end_render_pass(this.command_buffer);
            }
            Ok(())
        })?;

        // Read back the small edges buffer.
        let edges_extent_3d = vk::Extent3D {
            width: edges_extent.width,
            height: edges_extent.height,
            depth: 1,
        };
        self.read_image_to_staging(self.selection.edges.handle, edges_extent_3d)?;
        let len = (edges_extent.width as usize) * (edges_extent.height as usize);
        let bytes = self
            .staging
            .mapped()
            .ok_or(RendererError::StagingNotMapped)?;
        Ok(EdgesBuffer {
            bytes: bytes[..len].to_vec(),
            width: edges_extent.width,
            height: edges_extent.height,
        })
    }

    /// Read the full-resolution mask as a single-channel R8 byte buffer.
    /// Useful for saving / debugging.
    pub fn read_selection_mask(&mut self) -> Result<Vec<u8>, RendererError> {
        let extent = self.canvas.extent;
        self.read_image_to_staging(self.selection.mask.handle, extent)?;
        let len = usize::try_from(self.canvas_size.area()).expect("mask fits in usize");
        let bytes = self
            .staging
            .mapped()
            .ok_or(RendererError::StagingNotMapped)?;
        Ok(bytes[..len].to_vec())
    }

    /// Variant of [`Self::write_staging_to_image`] for R8 destinations.
    /// Identical body - present here to keep the selection-only path
    /// from depending on private layer-I/O helpers, and to document
    /// that the buffer holds tightly-packed R8 (1 byte per pixel).
    fn write_staging_to_r8_image(
        &mut self,
        image: vk::Image,
        extent: vk::Extent3D,
    ) -> Result<(), RendererError> {
        self.write_staging_to_image(image, extent)
    }
}
