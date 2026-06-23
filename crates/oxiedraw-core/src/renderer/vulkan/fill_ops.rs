//! Bucket-fill overlay GPU ops: upload the distance mask, record the
//! per-frame overlay pass, and clear state on commit/cancel.

use ash::vk;

use super::super::RendererError;
use super::super::fill_overlay::FILL_OVERLAY_PUSH_BYTES;
use super::VulkanRenderer;

impl VulkanRenderer {
    /// Upload the R8 distance mask produced by `flood_fill` into the
    /// canvas-sized overlay image. Called once at the start of a fill
    /// animation; the per-frame timer afterwards only updates the
    /// reveal push constant.
    pub fn upload_fill_mask(&mut self, mask: &[u8]) -> Result<(), RendererError> {
        {
            let staging = self
                .staging
                .mapped_mut()
                .ok_or(RendererError::StagingNotMapped)?;
            let copy_len = mask.len().min(staging.len());
            staging[..copy_len].copy_from_slice(&mask[..copy_len]);
        }
        let image = self.fill_overlay.mask.handle;
        let extent = self.canvas.extent;
        self.write_staging_to_image(image, extent)
    }

    /// Activate the fill overlay. The premultiplied colour and the
    /// owning layer index are captured here so `render_preview` can pick
    /// them up; per-frame radius updates go through `set_fill_reveal`.
    pub fn begin_fill_overlay(
        &mut self,
        layer_idx: usize,
        color_premul: [f32; 4],
    ) -> Result<(), RendererError> {
        if layer_idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        self.fill_active = true;
        self.fill_reveal = 0.0;
        self.fill_color_premul = color_premul;
        self.fill_layer_idx = layer_idx;
        Ok(())
    }

    /// Update only the reveal-radius push value. Cheap - the caller
    /// will trigger a present/preview render that re-binds the
    /// overlay pipeline with this value.
    pub const fn set_fill_reveal(&mut self, reveal: f32) {
        self.fill_reveal = reveal.clamp(0.0, 1.0);
    }

    /// Clear all fill-overlay state. Called by commit + cancel paths
    /// after the animation finishes (or is aborted).
    pub const fn clear_fill_overlay(&mut self) {
        self.fill_active = false;
        self.fill_reveal = 0.0;
    }

    /// Whether the fill-overlay path should be used by present/preview.
    #[must_use]
    pub const fn fill_active(&self) -> bool {
        self.fill_active
    }

    /// Render the preview image as: layer composite at the active fill
    /// layer's z-order with the fill overlay spliced in just above it.
    ///
    /// Mirrors `render_preview` but uses the fill overlay instead of
    /// the stroke overlay. `visibilities` follows the normal layer-
    /// visibility convention.
    pub fn render_fill_preview(&mut self, visibilities: &[bool]) -> Result<(), RendererError> {
        let visible_indices: Vec<usize> = visibilities
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| (v && i < self.layer_stack.slots.len()).then_some(i))
            .collect();
        let target_idx = self.fill_layer_idx;
        let push_color = self.fill_color_premul;
        let reveal = self.fill_reveal;

        let overlay_at = visible_indices.contains(&target_idx).then_some(target_idx);
        self.record_and_submit(|this| {
            let preview_img = this.preview.handle;
            let preview_fb = this.preview_framebuffer;
            this.cmd_clear_image(this.preview.handle, [0.0, 0.0, 0.0, 0.0]);
            for &idx in &visible_indices {
                if overlay_at == Some(idx) {
                    // Build (target layer + fill overlay) in a scratch, then
                    // blend it over the preview at the target's mode + opacity.
                    let scratch = this.erase_preview.scratch.handle;
                    let scratch_fb = this.erase_preview.framebuffer;
                    let layer_image = this.layer_stack.slots[idx].image.handle;
                    this.cmd_copy_image_full(layer_image, scratch);
                    this.cmd_compose_fill_overlay(scratch_fb, push_color, reveal);
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

    /// Fill overlay pass - binds the overlay descriptor set, pushes
    /// the colour + reveal radius, draws the fullscreen triangle into
    /// `framebuffer` (the target-plus-overlay scratch).
    fn cmd_compose_fill_overlay(
        &mut self,
        framebuffer: vk::Framebuffer,
        color: [f32; 4],
        reveal: f32,
    ) {
        let render_pass = self.canvas_target.render_pass;
        let pipeline = self.fill_overlay.pipeline;
        let layout = self.fill_overlay.layout;
        let descriptor_set = self.fill_overlay.descriptor_set;
        // Match the shader's std430-like push block: vec4 color then
        // float reveal (with vec4 alignment, GLSL packs the float at
        // offset 16).
        let push: [f32; 5] = [color[0], color[1], color[2], color[3], reveal];
        debug_assert_eq!(std::mem::size_of_val(&push) as u32, FILL_OVERLAY_PUSH_BYTES);
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
