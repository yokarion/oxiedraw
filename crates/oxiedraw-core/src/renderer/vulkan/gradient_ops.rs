//! Gradient overlay GPU ops: drag-time live preview and final commit-to-
//! layer. The ramp is evaluated in the fragment shader from the drag
//! endpoints + the baked LUT, so per-frame work is one fullscreen pass
//! regardless of canvas size.

use ash::vk;

use super::super::RendererError;
use super::super::gradient_overlay::{GRADIENT_PUSH_BYTES, LUT_WIDTH};
use super::VulkanRenderer;

/// Gradient ramp geometry. Must stay in lockstep with the `KIND_*`
/// constants in `gradient.frag` and the `GradientType` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradientKind {
    Linear = 0,
    Radial = 1,
    Square = 2,
}

/// Convert an `f32` to IEEE half-float bits. Gradient LUT values are
/// premultiplied linear in `[0, 1]`, so subnormals flush to zero and there
/// are no infinities to worry about.
fn f32_to_f16_bits(f: f32) -> u16 {
    let bits = f.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = bits & 0x007f_ffff;
    if exp <= 0 {
        return sign;
    }
    if exp >= 0x1f {
        return sign | 0x7c00;
    }
    #[allow(clippy::cast_possible_truncation)]
    let out = sign | ((exp as u16) << 10) | ((mant >> 13) as u16);
    out
}

impl VulkanRenderer {
    /// Arm the gradient overlay: subsequent `render_gradient_preview` calls
    /// splice the ramp on top of `layer_idx` at the right z-order. Call once
    /// at drag start; `set_gradient_preview_params` per drag move.
    pub const fn begin_gradient_overlay(&mut self, layer_idx: usize) {
        self.gradient_active = true;
        self.gradient_layer_idx = layer_idx;
        self.gradient_endpoints = [0.0; 4];
        self.gradient_extra = [0.0; 4];
    }

    /// Upload the baked LUT (`bake_lut`, 256 premultiplied-linear RGBA
    /// floats). Called once per drag; the ramp geometry then rides push
    /// constants only.
    pub fn set_gradient_lut(&mut self, lut: &[f32]) -> Result<(), RendererError> {
        let texels = (lut.len() / 4).min(LUT_WIDTH as usize);
        {
            let staging = self
                .staging
                .mapped_mut()
                .ok_or(RendererError::StagingNotMapped)?;
            for (i, &v) in lut.iter().take(texels * 4).enumerate() {
                let half = f32_to_f16_bits(v);
                let off = i * 2;
                staging[off] = (half & 0xff) as u8;
                staging[off + 1] = (half >> 8) as u8;
            }
        }
        let image = self.gradient_overlay.lut.handle;
        self.write_staging_to_image(
            image,
            vk::Extent3D { width: LUT_WIDTH, height: 1, depth: 1 },
        )
    }

    /// Update the ramp parameters drawn each subsequent preview frame.
    /// `endpoints` is `(x0, y0, x1, y1)` in canvas pixels.
    pub const fn set_gradient_preview_params(&mut self, kind: GradientKind, endpoints: [f32; 4]) {
        self.gradient_endpoints = endpoints;
        let sel = if self.selection_active { 1.0 } else { 0.0 };
        self.gradient_extra = [kind as i32 as f32, sel, 0.0, 0.0];
    }

    pub const fn clear_gradient_overlay(&mut self) {
        self.gradient_active = false;
    }

    #[must_use]
    pub const fn gradient_active(&self) -> bool {
        self.gradient_active
    }

    /// Render the preview image: visible layers composited up to the target
    /// layer, the gradient spliced in, then the layers above. Mirrors
    /// `render_shape_preview`.
    pub fn render_gradient_preview(&mut self, visibilities: &[bool]) -> Result<(), RendererError> {
        let visible_indices: Vec<usize> = visibilities
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| (v && i < self.layer_stack.slots.len()).then_some(i))
            .collect();
        let target_idx = self.gradient_layer_idx;
        let endpoints = self.gradient_endpoints;
        let extra = self.gradient_extra;

        let overlay_at = visible_indices.contains(&target_idx).then_some(target_idx);
        self.record_and_submit(|this| {
            let preview_img = this.preview.handle;
            let preview_fb = this.preview_framebuffer;
            this.cmd_clear_image(this.preview.handle, [0.0, 0.0, 0.0, 0.0]);
            for &idx in &visible_indices {
                if overlay_at == Some(idx) {
                    let scratch = this.erase_preview.scratch.handle;
                    let scratch_fb = this.erase_preview.framebuffer;
                    let layer_image = this.layer_stack.slots[idx].image.handle;
                    this.cmd_copy_image_full(layer_image, scratch);
                    this.record_gradient_pass_into(scratch_fb, endpoints, extra);
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

    /// Final commit: render the gradient directly into the layer's
    /// framebuffer with OVER blend. Clears the overlay state. Caller is
    /// responsible for `recomposite_canvas` afterwards.
    pub fn commit_gradient(
        &mut self,
        layer_idx: usize,
        kind: GradientKind,
        endpoints: [f32; 4],
    ) -> Result<(), RendererError> {
        if layer_idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        let sel = if self.selection_active { 1.0 } else { 0.0 };
        let extra = [kind as i32 as f32, sel, 0.0, 0.0];
        let layer_image = self.layer_stack.slots[layer_idx].image.handle;
        let framebuffer = self.layer_stack.slots[layer_idx].framebuffer;

        self.gradient_active = false;

        self.record_and_submit(|this| {
            this.barrier(layer_image, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            this.record_gradient_pass_into(framebuffer, endpoints, extra);
            Ok(())
        })?;
        self.layer_stack.touch(layer_idx);
        self.invalidate_preview_cache();
        Ok(())
    }

    /// Begin a fullscreen pass against `framebuffer`, bind the gradient
    /// pipeline + descriptor set, push endpoints + extra, draw.
    fn record_gradient_pass_into(
        &mut self,
        framebuffer: vk::Framebuffer,
        endpoints: [f32; 4],
        extra: [f32; 4],
    ) {
        let render_pass = self.canvas_target.render_pass;
        let pipeline = self.gradient_overlay.pipeline;
        let layout = self.gradient_overlay.layout;
        let descriptor_set = self.gradient_overlay.descriptor_set;

        let mut push = [0_f32; 8];
        push[0..4].copy_from_slice(&endpoints);
        push[4..8].copy_from_slice(&extra);
        debug_assert_eq!(std::mem::size_of_val(&push) as u32, GRADIENT_PUSH_BYTES);

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
