//! Pattern-tool overlay GPU ops: upload the generated pattern, preview it at
//! the target layer's z-order, and commit it into that layer.
//!
//! The pattern is rasterised on the CPU (the generator lives in
//! `oxiedraw-patterns`), so the only GPU work is one textured fullscreen pass.
//! Preview and commit run that same pass - the preview into a copy of the target
//! layer, the commit into the layer itself - so what the canvas shows while the
//! curve is being edited is what lands on Apply.

use ash::vk;

use super::super::RendererError;
use super::super::pattern_overlay::PatternOverlay;
use super::VulkanRenderer;
use crate::document::CompositeStep;

impl VulkanRenderer {
    /// Allocate the overlay on first use, primed to `GENERAL` so the first
    /// clear-and-upload is well defined.
    fn ensure_pattern_overlay(&mut self) -> Result<(), RendererError> {
        if self.pattern_overlay.is_some() {
            return Ok(());
        }
        let extent = self.canvas_extent_2d();
        let overlay = PatternOverlay::new(
            &self.device,
            &mut self.allocator,
            extent,
            self.canvas_target.render_pass,
            self.layer_composite_pipeline.descriptor_set_layout,
            self.layer_composite_pipeline.sampler,
            self.layer_composite_pipeline.layout,
        )?;
        let handle = overlay.image.handle;
        self.pattern_overlay = Some(overlay);
        self.record_and_submit(|this| {
            this.barrier(handle, vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL);
            Ok(())
        })
    }

    /// Arm the overlay over `layer_idx`: subsequent presents splice the uploaded
    /// pattern in at that layer's z-order. Idempotent, so the tool can re-arm on
    /// every upload and the preview follows whichever layer is active.
    pub fn begin_pattern_overlay(&mut self, layer_idx: usize) -> Result<(), RendererError> {
        self.ensure_pattern_overlay()?;
        self.pattern_overlay_active = true;
        self.pattern_overlay_layer_idx = layer_idx;
        Ok(())
    }

    /// Replace the overlay's contents with a `w x h` premultiplied BGRA block
    /// landing at `(x, y)` in canvas pixels. The rest of the image is cleared,
    /// so a shrinking pattern cannot leave the previous one's tail behind. The
    /// rect is clamped to the canvas.
    ///
    /// Clear and copy go in one submit: this runs per motion event while a node
    /// is being dragged, and each submit costs a fence wait.
    pub fn upload_pattern_overlay(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        pixels: &[u8],
    ) -> Result<(), RendererError> {
        self.ensure_pattern_overlay()?;
        let image = self
            .pattern_overlay
            .as_ref()
            .ok_or(RendererError::PatternOverlayMissing)?
            .image
            .handle;
        let region = self.stage_image_region(x, y, w, h, pixels)?;
        self.record_and_submit(|this| {
            this.cmd_clear_image(image, [0.0, 0.0, 0.0, 0.0]);
            if let Some((x, y, w, h)) = region {
                this.cmd_copy_staging_region_to_image(image, x, y, w, h);
            }
            Ok(())
        })
    }

    pub const fn clear_pattern_overlay(&mut self) {
        self.pattern_overlay_active = false;
    }

    /// Whether the present path should splice the pattern in.
    #[must_use]
    pub const fn pattern_overlay_active(&self) -> bool {
        self.pattern_overlay_active
    }

    /// The layer the pattern will land on. Lets the display path pick the
    /// folder-scoped preview when an adjustment must clip around it.
    #[must_use]
    pub const fn pattern_overlay_target(&self) -> usize {
        self.pattern_overlay_layer_idx
    }

    /// Render the preview image: visible layers composited up to the target
    /// layer, the pattern spliced in, then the layers above. Mirrors
    /// [`Self::render_shape_preview`].
    pub fn render_pattern_preview(&mut self, visibilities: &[bool]) -> Result<(), RendererError> {
        let visible_indices: Vec<usize> = visibilities
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| (v && i < self.layer_stack.slots.len()).then_some(i))
            .collect();
        let target_idx = self.pattern_overlay_layer_idx;

        let overlay_at = visible_indices.contains(&target_idx).then_some(target_idx);
        self.record_and_submit(|this| {
            let preview_img = this.preview.handle;
            let preview_fb = this.preview_framebuffer;
            this.cmd_clear_image(preview_img, [0.0, 0.0, 0.0, 0.0]);
            for &idx in &visible_indices {
                if overlay_at == Some(idx) {
                    // Flat path: only reached when nothing needs scoping, and a
                    // clipped layer always forces the scoped walk instead.
                    this.compose_pattern_target_into(preview_img, preview_fb, idx, None);
                } else {
                    this.preview_compose_layer(preview_img, preview_fb, idx);
                }
            }
            Ok(())
        })
    }

    /// Folder- and clip-aware preview: build (target layer + pattern) into the
    /// shared scratch, then walk the composite tree with that scratch standing
    /// in for the target, so adjustments, folder scope and clipping masks apply
    /// to the live pattern exactly as they will once it is applied. Mirrors
    /// [`Self::render_shape_preview_scoped`].
    pub fn render_pattern_preview_scoped(
        &mut self,
        steps: &[CompositeStep],
    ) -> Result<(), RendererError> {
        let target_idx = self.pattern_overlay_layer_idx;
        if target_idx >= self.layer_stack.slots.len() {
            return Ok(());
        }
        let scratch = self.erase_preview.scratch.handle;
        let scratch_fb = self.erase_preview.framebuffer;
        let layer_image = self.layer_stack.slots[target_idx].image.handle;
        self.record_and_submit(|this| {
            this.cmd_copy_image_full(layer_image, scratch);
            this.cmd_compose_pattern_into(scratch_fb);
            this.barrier(scratch, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            Ok(())
        })?;
        let target = self.replace_target_from_erase_scratch(target_idx);
        self.build_preview_scoped_multi(steps, &[(target_idx, target)])
    }

    /// Copy the target layer into scratch, splice the pattern on top, then blend
    /// the result over `acc` at the layer's own mode + opacity. Shared by the
    /// flat and folder-scoped previews.
    pub(super) fn compose_pattern_target_into(
        &self,
        acc_img: vk::Image,
        acc_fb: vk::Framebuffer,
        target_idx: usize,
        clip_set: Option<vk::DescriptorSet>,
    ) {
        let scratch = self.erase_preview.scratch.handle;
        let scratch_fb = self.erase_preview.framebuffer;
        let layer_image = self.layer_stack.slots[target_idx].image.handle;
        self.cmd_copy_image_full(layer_image, scratch);
        self.cmd_compose_pattern_into(scratch_fb);
        self.barrier(scratch, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        let (mode, opacity) = self.layer_stack.blend(target_idx);
        let set = self.erase_preview.composite_set;
        self.cmd_compose_layer_clipped(acc_img, acc_fb, set, mode, opacity, clip_set);
    }

    /// Final commit: run the same pass straight into the layer's framebuffer,
    /// so the pixels match the preview byte for byte. Clears the overlay.
    /// Caller is responsible for `recomposite_canvas` afterwards.
    pub fn commit_pattern(&mut self, layer_idx: usize) -> Result<(), RendererError> {
        if layer_idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        // Applying a disarmed overlay would lay the previous curve's pattern
        // down a second time - its pixels are still in the image.
        if !self.pattern_overlay_active || self.pattern_overlay.is_none() {
            return Err(RendererError::PatternOverlayMissing);
        }
        let layer_image = self.layer_stack.slots[layer_idx].image.handle;
        let framebuffer = self.layer_stack.slots[layer_idx].framebuffer;

        // Torn down up front so a failed submit can't leave the preview path
        // drawing a pattern that is now half-applied.
        self.pattern_overlay_active = false;

        self.record_and_submit(|this| {
            // Flush any prior reads of the layer (it is normally sampled by the
            // canvas composite) before writing into it through the render pass.
            this.barrier(layer_image, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            this.cmd_compose_pattern_into(framebuffer);
            Ok(())
        })?;
        self.layer_stack.touch(layer_idx);
        self.invalidate_preview_cache();
        Ok(())
    }

    /// One textured fullscreen pass: the uploaded pattern over `framebuffer`,
    /// premultiplied OVER - or the alpha-preserving variant when the target
    /// layer is alpha-locked.
    fn cmd_compose_pattern_into(&self, framebuffer: vk::Framebuffer) {
        let Some(overlay) = self.pattern_overlay.as_ref() else {
            return;
        };
        let render_pass = self.canvas_target.render_pass;
        let pipeline = if self.alpha_lock {
            overlay.pipeline_alpha_lock
        } else {
            self.layer_composite_pipeline.pipeline
        };
        let layout = self.layer_composite_pipeline.layout;
        self.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
        unsafe {
            self.device.cmd_bind_descriptor_sets(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                &[overlay.composite_set],
                &[],
            );
        }
        self.cmd_end_fullscreen_pass();
    }
}
