//! Builds the in-flight stroke preview image: layer composite with the
//! current stroke spliced in at the target layer's z-order.

use ash::vk;

use super::super::RendererError;
use super::super::dab::{DabFamily, DabInstance};
use super::VulkanRenderer;

impl VulkanRenderer {
    /// Build the in-flight stroke preview and copy it straight into the
    /// dmabuf display image in a single submit (one fence-wait instead of
    /// two). This is the per-motion-event drag path.
    pub fn render_preview_and_present(
        &mut self,
        visibilities: &[bool],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<(), RendererError> {
        let push: [f32; 4] = [
            color_linear[0],
            color_linear[1],
            color_linear[2],
            opacity.clamp(0.0, 1.0),
        ];
        let visible_indices = self.visible_layer_indices(visibilities);
        let display_old_layout = self.display_old_layout();
        self.record_and_submit(|this| {
            this.record_layered_preview(&visible_indices, target_idx, push);
            let preview_image = this.preview.handle;
            this.record_present_copy(preview_image, display_old_layout);
            Ok(())
        })?;
        self.display_initialised = true;
        Ok(())
    }

    /// Stamp `instances` into the stroke buffer, rebuild the in-flight
    /// preview, and copy it to the dmabuf display - all in ONE submit. This
    /// is the per-motion-event drag path: it replaces a separate `stamp`
    /// submit and `present` submit with a single fence-wait.
    pub fn stamp_preview_present(
        &mut self,
        family: DabFamily,
        instances: &[DabInstance],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
        visibilities: &[bool],
    ) -> Result<(), RendererError> {
        let n = if instances.is_empty() {
            0
        } else {
            self.accumulate_dirty(instances);
            self.dab_buffers.upload_instances(instances)?
        };
        let mask_pipe = self.mask_pipelines.get(family);
        let pipeline = mask_pipe.pipeline;
        let layout = mask_pipe.layout;
        let stroke_rp = self.stroke_target.render_pass;
        let stroke_fb = self.stroke_target.framebuffer;
        let push: [f32; 4] = [
            color_linear[0],
            color_linear[1],
            color_linear[2],
            opacity.clamp(0.0, 1.0),
        ];
        let visible_indices = self.visible_layer_indices(visibilities);
        let display_old_layout = self.display_old_layout();
        self.record_and_submit(|this| {
            if n > 0 {
                // Stamp the dab mask into the stroke buffer.
                this.cmd_dab_pass(family, pipeline, layout, stroke_rp, stroke_fb, n);
                // Make the mask writes visible to the preview composite's
                // sampler reads (RADV drops this silently otherwise).
                this.barrier(
                    this.stroke.handle,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::GENERAL,
                );
            }
            this.record_layered_preview(&visible_indices, target_idx, push);
            let preview_image = this.preview.handle;
            this.record_present_copy(preview_image, display_old_layout);
            Ok(())
        })?;
        self.display_initialised = true;
        Ok(())
    }

    /// Same as [`Self::render_preview_and_present`] but copies the preview
    /// into the staging buffer and returns a freshly allocated `Vec`.
    pub fn render_preview_layered_and_read(
        &mut self,
        visibilities: &[bool],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<Vec<u8>, RendererError> {
        self.record_preview_to_staging(visibilities, target_idx, color_linear, opacity)?;
        self.copy_staging_bytes()
    }

    /// Like [`Self::render_preview_layered_and_read`] but fills a
    /// caller-owned buffer instead of allocating.
    pub fn render_preview_layered_into(
        &mut self,
        visibilities: &[bool],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
        out: &mut Vec<u8>,
    ) -> Result<(), RendererError> {
        self.record_preview_to_staging(visibilities, target_idx, color_linear, opacity)?;
        self.copy_staging_bytes_into(out)
    }

    /// Build the layered preview and copy it into the staging buffer in
    /// one submit. Shared by the owned-`Vec` and caller-buffer readers.
    fn record_preview_to_staging(
        &mut self,
        visibilities: &[bool],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<(), RendererError> {
        let extent = self.canvas.extent;
        let push: [f32; 4] = [
            color_linear[0],
            color_linear[1],
            color_linear[2],
            opacity.clamp(0.0, 1.0),
        ];
        let visible_indices = self.visible_layer_indices(visibilities);
        self.record_and_submit(|this| {
            this.record_layered_preview(&visible_indices, target_idx, push);
            this.barrier(
                this.preview.handle,
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
                    this.preview.handle,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    this.staging.handle,
                    &[region],
                );
            }
            this.barrier(
                this.preview.handle,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::ImageLayout::GENERAL,
            );
            Ok(())
        })
    }

    pub(super) fn visible_layer_indices(&self, visibilities: &[bool]) -> Vec<usize> {
        visibilities
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| (v && i < self.layer_stack.slots.len()).then_some(i))
            .collect()
    }

    /// Compose the preview image from a list of visible layer indices,
    /// inserting the in-flight stroke directly above the target layer.
    /// Records into `self.command_buffer`; caller wraps in `record_and_submit`.
    ///
    /// The layers up to and including the target are cached in
    /// `preview_below` and only rebuilt when the cache is invalid (start
    /// of a stroke / after a layer mutation). Each event then just copies
    /// that cache into `preview`, composites the stroke, and re-composites
    /// the (usually few) layers above the target. `visible_indices` is
    /// ascending in slot order.
    fn record_layered_preview(
        &mut self,
        visible_indices: &[usize],
        target_idx: usize,
        stroke_push: [f32; 4],
    ) {
        let erase = self.stroke_erase;
        if !self.preview_cache_valid {
            let below_fb = self.preview_below_framebuffer;
            self.cmd_clear_image(self.preview_below.handle, [0.0, 0.0, 0.0, 0.0]);
            for &idx in visible_indices {
                // Erasing rebuilds the target each event (with the stroke
                // punched out), so the cache must exclude it; the brush
                // composites its stroke over a cache that includes it.
                if (erase && idx >= target_idx) || (!erase && idx > target_idx) {
                    break;
                }
                self.preview_compose_layer(below_fb, idx);
            }
            self.preview_cache_valid = true;
        }

        // preview := cached below stack.
        self.cmd_copy_image_full(self.preview_below.handle, self.preview.handle);

        // The in-flight stroke, spliced at the target layer's z-order (only
        // if the target is visible).
        if visible_indices.contains(&target_idx) {
            if erase {
                self.preview_compose_erased_target(target_idx, stroke_push);
            } else {
                self.cmd_composite_stroke(self.preview_framebuffer, stroke_push, false);
            }
        }

        // Layers above the target, re-composited each event.
        let preview_fb = self.preview_framebuffer;
        for &idx in visible_indices {
            if idx <= target_idx {
                continue;
            }
            self.preview_compose_layer(preview_fb, idx);
        }
    }

    pub(super) fn preview_compose_layer(&self, framebuffer: vk::Framebuffer, idx: usize) {
        let descriptor_set = self.layer_stack.slots[idx].descriptor_set;
        self.cmd_compose_image(framebuffer, descriptor_set);
    }

    /// Build the target layer with the stroke coverage punched out into the
    /// erase scratch, then composite that over the below-cache already in
    /// `preview`. This reveals the layers below where the eraser passed,
    /// without touching them.
    fn preview_compose_erased_target(&self, target_idx: usize, push: [f32; 4]) {
        let scratch = self.erase_preview.scratch.handle;
        let scratch_fb = self.erase_preview.framebuffer;
        // scratch := the target layer. Copy the layer image directly (one
        // transfer) instead of clearing + compositing it (the layer is already
        // a standalone premultiplied image, so a copy is identical and cheaper).
        let layer_image = self.layer_stack.slots[target_idx].image.handle;
        self.cmd_copy_image_full(layer_image, scratch);
        // Punch out the stroke coverage (DST_OUT) from the target copy.
        self.cmd_composite_stroke(scratch_fb, push, true);
        // Make the scratch writes visible to the sampler reads below.
        self.barrier(scratch, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        // erased target OVER the below-cache already in preview.
        let set = self.erase_preview.composite_set;
        self.cmd_compose_image(self.preview_framebuffer, set);
    }

    /// Composite one premultiplied BGRA image (via `descriptor_set`) onto
    /// `framebuffer` with the layer-composite pipeline (premultiplied OVER).
    pub(super) fn cmd_compose_image(
        &self,
        framebuffer: vk::Framebuffer,
        descriptor_set: vk::DescriptorSet,
    ) {
        let render_pass = self.canvas_target.render_pass;
        let pipeline = self.layer_composite_pipeline.pipeline;
        let layout = self.layer_composite_pipeline.layout;
        self.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
        unsafe {
            self.device.cmd_bind_descriptor_sets(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                &[descriptor_set],
                &[],
            );
        }
        self.cmd_end_fullscreen_pass();
    }
}
