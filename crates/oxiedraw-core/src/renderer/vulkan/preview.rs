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
        // Cache the layers strictly below the target, each with its blend mode
        // + opacity. The target (with its in-flight stroke merged) and the
        // layers above are re-composited every event.
        if !self.preview_cache_valid {
            let below_img = self.preview_below.handle;
            let below_fb = self.preview_below_framebuffer;
            self.cmd_clear_image(self.preview_below.handle, [0.0, 0.0, 0.0, 0.0]);
            for &idx in visible_indices {
                if idx >= target_idx {
                    break;
                }
                self.preview_compose_layer(below_img, below_fb, idx);
            }
            self.preview_cache_valid = true;
        }

        // preview := cached below stack.
        self.cmd_copy_image_full(self.preview_below.handle, self.preview.handle);

        // The target layer with the in-flight stroke merged in, blended at the
        // target's own mode + opacity (only if the target is visible).
        if visible_indices.contains(&target_idx) {
            self.preview_compose_stroked_target(target_idx, stroke_push, erase);
        }

        // Layers above the target, re-composited each event.
        let preview_img = self.preview.handle;
        let preview_fb = self.preview_framebuffer;
        for &idx in visible_indices {
            if idx <= target_idx {
                continue;
            }
            self.preview_compose_layer(preview_img, preview_fb, idx);
        }
    }

    /// Blend-composite layer `idx` onto the accumulator (`acc_img` / `acc_fb`)
    /// using its stored blend mode + opacity.
    pub(super) fn preview_compose_layer(
        &self,
        acc_img: vk::Image,
        acc_fb: vk::Framebuffer,
        idx: usize,
    ) {
        let descriptor_set = self.layer_stack.slots[idx].descriptor_set;
        let (mode, opacity) = self.layer_stack.blend(idx);
        self.cmd_compose_layer_blended(acc_img, acc_fb, descriptor_set, mode, opacity);
    }

    /// Build the target layer with the in-flight stroke merged into a scratch
    /// (OVER for paint, DST_OUT for erase), then blend that scratch over the
    /// below-cache already in `preview` using the target's mode + opacity. This
    /// keeps a non-Normal target layer's blend applied to its live stroke.
    fn preview_compose_stroked_target(&self, target_idx: usize, push: [f32; 4], erase: bool) {
        let scratch = self.erase_preview.scratch.handle;
        let scratch_fb = self.erase_preview.framebuffer;
        // scratch := the target layer pixels (a copy is identical to a clear +
        // OVER-composite of the standalone premultiplied layer, and cheaper).
        let layer_image = self.layer_stack.slots[target_idx].image.handle;
        self.cmd_copy_image_full(layer_image, scratch);
        // Merge the stroke into the target copy.
        self.cmd_composite_stroke(scratch_fb, push, erase);
        // Make the scratch writes visible to the blend pass's sampler reads.
        self.barrier(scratch, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        let (mode, opacity) = self.layer_stack.blend(target_idx);
        let set = self.erase_preview.composite_set;
        self.cmd_compose_layer_blended(
            self.preview.handle,
            self.preview_framebuffer,
            set,
            mode,
            opacity,
        );
    }
}
