//! Layer-level operations: add / remove / reorder / clear, compositing
//! the stroke into a target layer, and rebuilding the canvas image from
//! the visible layer stack.

use ash::vk;

use super::super::RendererError;
use super::{CANVAS_FORMAT, VulkanRenderer, full_image_barrier};

impl VulkanRenderer {
    /// Monotonic content version of layer `idx`, bumped on every write to its
    /// image. Lets callers (the layers panel) re-read only changed layers.
    #[must_use]
    pub fn layer_content_version(&self, idx: usize) -> u64 {
        self.layer_stack.version(idx)
    }

    /// Set layer `idx`'s blend-mode index (matches `layer_blend.frag`) and
    /// opacity. Pure GPU-side metadata; caller re-composites the canvas.
    pub fn set_layer_blend(&mut self, idx: usize, mode: u32, opacity: f32) {
        self.layer_stack.set_blend(idx, mode, opacity);
        // The cached below-stack may include this layer; force a rebuild.
        self.preview_cache_valid = false;
        self.scoped_cache_valid = false;
    }

    /// Append a new layer image and clear it to fully transparent.
    pub fn add_layer(&mut self) -> Result<usize, RendererError> {
        let extent = vk::Extent2D {
            width: self.canvas_size.width,
            height: self.canvas_size.height,
        };
        let idx = self.layer_stack.add(
            &self.device,
            &mut self.allocator,
            &self.layer_composite_pipeline,
            self.canvas_target.render_pass,
            extent,
            CANVAS_FORMAT,
        )?;
        self.transition_layer_initial(idx)?;
        self.clear_layer(idx, [0.0, 0.0, 0.0, 0.0])?;
        self.liquify_shift_for_insert(idx);
        Ok(idx)
    }

    /// Remove the layer at `idx`.
    pub fn remove_layer(&mut self, idx: usize) -> Result<(), RendererError> {
        // A live liquify session pins its target by slot index. Remap it here
        // rather than making every caller remember to close the tool first.
        if self.liquify_shift_for_remove(idx) {
            self.end_liquify();
        }
        unsafe { self.device.device_wait_idle()? };
        unsafe {
            self.layer_stack
                .remove(idx, &self.device, &mut self.allocator)?;
        }
        Ok(())
    }

    /// Move the layer at `from` to position `to`. Metadata-only.
    pub fn reorder_layer(&mut self, from: usize, to: usize) {
        self.liquify_shift_for_reorder(from, to);
        self.layer_stack.reorder(from, to);
    }

    /// Number of layers currently allocated on the GPU.
    #[must_use]
    pub fn layer_count(&self) -> usize {
        self.layer_stack.slots.len()
    }

    /// Clear the layer at `idx` to `color`.
    pub fn clear_layer(&mut self, idx: usize, color: [f32; 4]) -> Result<(), RendererError> {
        if idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        self.record_and_submit(|this| {
            let image = this.layer_stack.slots[idx].image.handle;
            this.cmd_clear_image(image, color);
            Ok(())
        })?;
        self.layer_stack.touch(idx);
        Ok(())
    }

    pub(super) fn transition_layer_initial(&mut self, idx: usize) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            let image = this.layer_stack.slots[idx].image.handle;
            unsafe {
                this.device.cmd_pipeline_barrier(
                    this.command_buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[full_image_barrier(
                        image,
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::GENERAL,
                    )],
                );
            }
            Ok(())
        })
    }

    /// Commit the in-flight stroke in a single submit: composite the
    /// stroke buffer into the target layer, clear the stroke buffer, and
    /// rebuild the canvas from the visible layer stack. Replaces three
    /// separate submits (one fence-wait instead of three).
    pub fn commit_stroke_into_layer(
        &mut self,
        layer_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
        visibilities: &[bool],
    ) -> Result<(), RendererError> {
        if layer_idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        let framebuffer = self.layer_stack.slots[layer_idx].framebuffer;
        let layer_image = self.layer_stack.slots[layer_idx].image.handle;
        let push: [f32; 4] = [
            color_linear[0],
            color_linear[1],
            color_linear[2],
            opacity.clamp(0.0, 1.0),
        ];
        let visible_indices = self.visible_layer_indices(visibilities);
        // Adjustment layers run a multi-submit effect chain, so their canvas
        // rebuild can't share this single submit. Without an adjustment the
        // canvas rebuild stays folded into the stroke-commit submit (the fast,
        // common path).
        let has_adjustments = self.has_adjustment_layers();
        self.record_and_submit(|this| {
            // 1. Stroke buffer -> target layer.
            this.cmd_composite_pass(framebuffer, push);
            // 2. Make the layer's new pixels visible to the canvas
            //    re-composite's sampler reads (RADV drops this silently
            //    without an explicit barrier).
            this.barrier(layer_image, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            // 3. Reset the stroke buffer for the next stroke / stamp.
            this.cmd_clear_image(this.stroke.handle, [0.0, 0.0, 0.0, 0.0]);
            // 4. Rebuild the canvas from the layer stack (no-adjustment path).
            if !has_adjustments {
                this.cmd_composite_layers_to_canvas(&visible_indices);
            }
            Ok(())
        })?;
        self.layer_stack.touch(layer_idx);
        // 4b. With adjustment layers present, rebuild the canvas through the
        //     per-layer-submit path so each effect's chain applies to the
        //     accumulator instead of the slot being drawn as its raw mask.
        if has_adjustments {
            self.composite_layers_to_canvas_adjusted(&visible_indices)?;
        }
        Ok(())
    }

    /// Composite only the visible layers strictly above `target_idx` into
    /// the (transient) preview image and read them back as BGRA8 (row-major,
    /// no padding). The transform tool draws this over its live preview so
    /// the transformed layer stays in its z-order instead of floating on top.
    pub fn read_layers_above(
        &mut self,
        visibilities: &[bool],
        target_idx: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), RendererError> {
        let visible_indices = self.visible_layer_indices(visibilities);
        let preview_img = self.preview.handle;
        let preview_fb = self.preview_framebuffer;
        self.record_and_submit(|this| {
            this.cmd_clear_image(this.preview.handle, [0.0, 0.0, 0.0, 0.0]);
            for &idx in &visible_indices {
                if idx <= target_idx {
                    continue;
                }
                this.preview_compose_layer(preview_img, preview_fb, idx);
            }
            Ok(())
        })?;
        let extent = self.canvas.extent;
        self.read_image_to_staging(self.preview.handle, extent)?;
        self.copy_staging_bytes_into(out)
    }

    /// Composite the given layer indices (bottom-up, each at its own blend
    /// mode + opacity) over transparent into the preview image and read the
    /// result back as BGRA8. Used by layer merge so the flattened raster
    /// matches what the layers looked like composited together.
    pub fn read_layers_composited(
        &mut self,
        indices: &[usize],
        out: &mut Vec<u8>,
    ) -> Result<(), RendererError> {
        for &idx in indices {
            if idx >= self.layer_stack.slots.len() {
                return Err(RendererError::LayerIndexOutOfRange);
            }
        }
        let preview_img = self.preview.handle;
        let preview_fb = self.preview_framebuffer;
        self.record_and_submit(|this| {
            this.cmd_clear_image(preview_img, [0.0, 0.0, 0.0, 0.0]);
            for &idx in indices {
                this.preview_compose_layer(preview_img, preview_fb, idx);
            }
            Ok(())
        })?;
        let extent = self.canvas.extent;
        self.read_image_to_staging(self.preview.handle, extent)?;
        self.copy_staging_bytes_into(out)
    }

    /// Rebuild the canvas image from the layer stack. Clear to
    /// transparent, then composite each visible layer bottom-up.
    pub fn composite_layers_to_canvas(
        &mut self,
        visibilities: &[bool],
    ) -> Result<(), RendererError> {
        let visible_indices = self.visible_layer_indices(visibilities);
        // Adjustment layers run multi-pass effect chains that can't share a
        // single command submission, so fall back to the per-layer path.
        if self.has_adjustment_layers() {
            return self.composite_layers_to_canvas_adjusted(&visible_indices);
        }
        self.record_and_submit(|this| {
            this.cmd_composite_layers_to_canvas(&visible_indices);
            Ok(())
        })
    }

    /// Rebuild the canvas image from only the visible layers up to and
    /// including `target_idx`. The transform tool uses this so the base
    /// canvas excludes the upper layers (which it draws as a separate
    /// overlay), avoiding a double-composite of semi-transparent upper
    /// layers under the live preview.
    pub fn composite_layers_below_to_canvas(
        &mut self,
        visibilities: &[bool],
        target_idx: usize,
    ) -> Result<(), RendererError> {
        let visible_indices: Vec<usize> = self
            .visible_layer_indices(visibilities)
            .into_iter()
            .filter(|&i| i <= target_idx)
            .collect();
        self.record_and_submit(|this| {
            this.cmd_composite_layers_to_canvas(&visible_indices);
            Ok(())
        })
    }

    /// Record (no submit) the canvas rebuild: clear to transparent, then
    /// composite each visible layer bottom-up. Caller wraps in
    /// `record_and_submit`.
    pub(super) fn cmd_composite_layers_to_canvas(&self, visible_indices: &[usize]) {
        self.cmd_clear_image(self.canvas.handle, [0.0, 0.0, 0.0, 0.0]);
        let canvas_img = self.canvas.handle;
        let canvas_fb = self.canvas_target.framebuffer;
        for &idx in visible_indices {
            let descriptor_set = self.layer_stack.slots[idx].descriptor_set;
            let (mode, opacity) = self.layer_stack.blend(idx);
            self.cmd_compose_layer_blended(canvas_img, canvas_fb, descriptor_set, mode, opacity);
        }
    }

    /// Shared inner of the stroke-composite render pass, submitted on its
    /// own. `framebuffer` is the canvas's (via `composite_stroke`) or a
    /// layer's (via `commit_stroke_into_layer`).
    pub(super) fn record_composite_pass(
        &mut self,
        framebuffer: vk::Framebuffer,
        push: [f32; 4],
    ) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            this.cmd_composite_pass(framebuffer, push);
            Ok(())
        })
    }

    /// Record (no submit) the commit-time stroke-composite render pass into
    /// `framebuffer`. Uses the erase (DST_OUT) pipeline when `self.stroke_erase`
    /// is set, so the stroke removes coverage from the target instead of adding
    /// tinted color.
    pub(super) fn cmd_composite_pass(&self, framebuffer: vk::Framebuffer, push: [f32; 4]) {
        self.cmd_composite_stroke(framebuffer, push, self.stroke_erase);
    }

    /// Record (no submit) a stroke-composite pass: sample the stroke buffer
    /// (+ selection mask) and blend it into `framebuffer` at the pushed tint /
    /// opacity. `erase` selects the DST_OUT pipeline (remove coverage) over the
    /// OVER pipeline (add tinted color). The fifth push value is the
    /// selection-active flag. Shared by the commit and preview paths.
    pub(super) fn cmd_composite_stroke(
        &self,
        framebuffer: vk::Framebuffer,
        push: [f32; 4],
        erase: bool,
    ) {
        let selection_active: f32 = if self.selection_active { 1.0 } else { 0.0 };
        let push_full: [f32; 5] = [push[0], push[1], push[2], push[3], selection_active];
        let render_pass = self.canvas_target.render_pass;
        let pipeline = if erase {
            self.composite_pipeline.erase_pipeline
        } else {
            self.composite_pipeline.pipeline
        };
        let layout = self.composite_pipeline.layout;
        let descriptor_set = self.composite_pipeline.descriptor_set;
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
            let push_bytes = std::slice::from_raw_parts(
                push_full.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&push_full),
            );
            self.device.cmd_push_constants(
                self.command_buffer,
                layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                push_bytes,
            );
        }
        self.cmd_end_fullscreen_pass();
    }
}
