//! Shape overlay GPU ops: drag-time live preview and final commit-to-
//! layer. The shape itself is computed SDF-style in the fragment shader
//! from push constants alone, so per-frame work is bounded by canvas
//! resolution (one fullscreen pass) regardless of shape size.

use ash::vk;

use super::super::RendererError;
use super::super::shape_overlay::SHAPE_PUSH_BYTES;
use super::VulkanRenderer;

/// Shape primitive kind. Must stay in lockstep with the `KIND_*`
/// constants in `shape.frag` and the `ShapeTool` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    Rectangle = 0,
    Circle = 1,
    Triangle = 2,
    Line = 3,
}

impl ShapeKind {
    /// Pack a drag-defined bounding box `(x, y, w, h)` into the 4-float
    /// layout `shape.frag` expects. Box shapes pass straight through; a
    /// Line is encoded as its two endpoints `(x0, y0, x1, y1)`, derived
    /// from the box diagonal (the box `w`/`h` keep their sign so the line
    /// runs from `(x, y)` to `(x + w, y + h)`).
    #[must_use]
    pub fn pack_drag_rect(self, rect: (f32, f32, f32, f32)) -> [f32; 4] {
        let (x, y, w, h) = rect;
        match self {
            Self::Line => [x, y, x + w, y + h],
            _ => [x, y, w, h],
        }
    }
}

impl VulkanRenderer {
    /// Arm the shape overlay path: subsequent `render_shape_preview`
    /// calls will splice the shape on top of `layer_idx` at the right
    /// z-order. Call once at drag start; `set_shape_preview_params`
    /// per drag move.
    pub const fn begin_shape_overlay(&mut self, layer_idx: usize) {
        self.shape_active = true;
        self.shape_layer_idx = layer_idx;
        // Default to an empty rect so a stray present before the first
        // set_shape_preview_params draws nothing meaningful.
        self.shape_color_premul = [0.0; 4];
        self.shape_rect = [0.0; 4];
        self.shape_extra = [0.0; 4];
    }

    /// Update the shape parameters drawn each subsequent preview frame.
    ///
    /// `rect` is `(x, y, w, h)` for box shapes and `(x0, y0, x1, y1)`
    /// for `ShapeKind::Line`. `line_width` is only consulted for Line.
    pub const fn set_shape_preview_params(
        &mut self,
        kind: ShapeKind,
        rect: [f32; 4],
        color_premul: [f32; 4],
        antialias: bool,
        line_width: f32,
    ) {
        self.shape_rect = rect;
        self.shape_color_premul = color_premul;
        let aa = if antialias { 1.0 } else { 0.0 };
        let sel = if self.selection_active { 1.0 } else { 0.0 };
        self.shape_extra = [kind as i32 as f32, aa, line_width, sel];
    }

    pub const fn clear_shape_overlay(&mut self) {
        self.shape_active = false;
    }

    #[must_use]
    pub const fn shape_active(&self) -> bool {
        self.shape_active
    }

    /// Render the preview image as: visible layers composited up to
    /// the target layer, then the shape spliced in, then the layers
    /// above. Mirrors `render_fill_preview`.
    pub fn render_shape_preview(&mut self, visibilities: &[bool]) -> Result<(), RendererError> {
        let visible_indices: Vec<usize> = visibilities
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| (v && i < self.layer_stack.slots.len()).then_some(i))
            .collect();
        let target_idx = self.shape_layer_idx;
        let color = self.shape_color_premul;
        let rect = self.shape_rect;
        let extra = self.shape_extra;

        let overlay_at = visible_indices.contains(&target_idx).then_some(target_idx);
        self.record_and_submit(|this| {
            let preview_img = this.preview.handle;
            let preview_fb = this.preview_framebuffer;
            this.cmd_clear_image(this.preview.handle, [0.0, 0.0, 0.0, 0.0]);
            for &idx in &visible_indices {
                if overlay_at == Some(idx) {
                    // Build (target layer + shape) in a scratch, then blend it
                    // over the preview at the target's mode + opacity.
                    let scratch = this.erase_preview.scratch.handle;
                    let scratch_fb = this.erase_preview.framebuffer;
                    let layer_image = this.layer_stack.slots[idx].image.handle;
                    this.cmd_copy_image_full(layer_image, scratch);
                    this.record_shape_pass_into(scratch_fb, color, rect, extra);
                    this.barrier(scratch, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
                    let (mode, opacity) = this.layer_stack.blend(idx);
                    let set = this.erase_preview.composite_set;
                    this.cmd_compose_layer_blended(preview_img, preview_fb, set, mode, opacity);
                } else {
                    this.preview_compose_layer(preview_img, preview_fb, idx);
                }
            }
            Ok(())
        })
    }

    /// Final commit: render the shape directly into the layer's
    /// framebuffer with OVER blend. Clears the overlay state.
    /// Caller is responsible for `recomposite_canvas` afterwards.
    pub fn commit_shape(
        &mut self,
        layer_idx: usize,
        kind: ShapeKind,
        rect: [f32; 4],
        color_premul: [f32; 4],
        antialias: bool,
        line_width: f32,
    ) -> Result<(), RendererError> {
        if layer_idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        let aa = if antialias { 1.0 } else { 0.0 };
        let sel = if self.selection_active { 1.0 } else { 0.0 };
        let extra = [kind as i32 as f32, aa, line_width, sel];
        let layer_image = self.layer_stack.slots[layer_idx].image.handle;
        let framebuffer = self.layer_stack.slots[layer_idx].framebuffer;

        // Tear the overlay down up front so the preview path can't keep
        // drawing a stale shape if the submit below fails.
        self.shape_active = false;

        self.record_and_submit(|this| {
            // Flush any prior reads of the layer (it's normally sampled
            // by the canvas composite pipeline) before we write into it
            // through the render pass. Layout stays GENERAL - the
            // render pass attachment is configured for GENERAL in/out.
            this.barrier(
                layer_image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
            );
            this.record_shape_pass_into(framebuffer, color_premul, rect, extra);
            Ok(())
        })?;
        self.layer_stack.touch(layer_idx);
        self.invalidate_preview_cache();
        Ok(())
    }

    /// Begin a fullscreen render pass against `framebuffer`, bind the
    /// shape pipeline, push the params, draw the fullscreen triangle.
    fn record_shape_pass_into(
        &mut self,
        framebuffer: vk::Framebuffer,
        color: [f32; 4],
        rect: [f32; 4],
        extra: [f32; 4],
    ) {
        let render_pass = self.canvas_target.render_pass;
        let pipeline = self.shape_overlay.pipeline;
        let layout = self.shape_overlay.layout;
        let descriptor_set = self.shape_overlay.descriptor_set;

        let mut push = [0_f32; 12];
        push[0..4].copy_from_slice(&color);
        push[4..8].copy_from_slice(&rect);
        push[8..12].copy_from_slice(&extra);
        debug_assert_eq!(std::mem::size_of_val(&push) as u32, SHAPE_PUSH_BYTES);

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
                push.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&push),
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
