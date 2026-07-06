//! Public Vulkan operations for layer filters.
//!
//! A filter turns one layer into a fully-filtered (and mask-mixed) copy by
//! running a short chain of fullscreen passes that ping-pong between the two
//! filter scratch images. Each pass is its own fence-waited submission, so
//! the single shared input descriptor set can be rewritten between passes
//! and a barrier on each source guarantees the previous pass's writes are
//! visible.
//!
//! - Live preview: [`Self::render_filter_preview`] composites the visible
//!   layers into the preview image, substituting the filtered scratch for
//!   each affected layer. The layer images themselves are never touched, so
//!   cancelling a filter is a no-op.
//! - Apply: [`Self::apply_filter_to_layer`] runs the same chain and copies
//!   the result back into the layer image.

use ash::vk;

use crate::document::CompositeStep;
use crate::filters::FilterSpec;

use super::super::RendererError;
use super::super::filters::{JfaSlot, Scratch};
use super::adjust_ops::PreviewTarget;
use super::VulkanRenderer;

impl VulkanRenderer {
    /// Arm the filter preview path for `affected` layers with `spec`.
    pub fn begin_filter(&mut self, affected: Vec<usize>, spec: FilterSpec) {
        self.filter_active = true;
        self.filter_affected = affected;
        self.filter_spec = spec;
        // Drop any stale per-stroke static-folder cache the scoped preview reuses.
        self.invalidate_preview_cache();
    }

    /// Update the previewed parameters (slider moved). Cheap - the next
    /// preview render picks it up.
    pub const fn update_filter_spec(&mut self, spec: FilterSpec) {
        self.filter_spec = spec;
    }

    /// Disarm the filter preview path. Layer images are untouched, so this
    /// is all that cancel needs to do.
    pub fn clear_filter(&mut self) {
        self.filter_active = false;
        self.filter_affected.clear();
    }

    #[must_use]
    pub const fn filter_active(&self) -> bool {
        self.filter_active
    }

    /// The single affected layer when exactly one layer is filtered, else
    /// `None`. Used to route the live preview through the folder-scoped
    /// composite when an adjustment is in play.
    #[must_use]
    pub fn filter_single_target(&self) -> Option<usize> {
        match self.filter_affected.as_slice() {
            [idx] => Some(*idx),
            _ => None,
        }
    }

    /// Folder-scoped / adjustment-aware filter preview: run the target layer's
    /// filter chain, then walk the composite tree with the filtered scratch
    /// spliced in at the target so an adjustment above (or below) clips exactly
    /// like the committed recomposite. Mirrors
    /// [`Self::render_gradient_preview_scoped`]; the flat
    /// [`Self::render_filter_preview`] is used when no adjustment is in play.
    pub fn render_filter_preview_scoped(
        &mut self,
        steps: &[CompositeStep],
        target_idx: usize,
    ) -> Result<(), RendererError> {
        let spec = self.filter_spec;
        let result = self.produce_filtered_layer(target_idx, spec)?;
        let src_img = self.filter_resources.scratch_handle(result);
        let set = self.filter_resources.composite_set(result);
        let (mode, opacity) = self.layer_stack.blend(target_idx);
        self.build_preview_scoped(
            steps,
            target_idx,
            PreviewTarget::Filter { src_img, set, mode, opacity },
        )
    }

    /// As [`Self::render_filter_preview_scoped`] but reads the preview back to
    /// host memory (tests / diagnostics) instead of presenting it.
    pub fn read_filter_preview_scoped(
        &mut self,
        steps: &[CompositeStep],
        target_idx: usize,
    ) -> Result<Vec<u8>, RendererError> {
        self.render_filter_preview_scoped(steps, target_idx)?;
        let extent = self.canvas.extent;
        self.read_image_to_staging(self.preview.handle, extent)?;
        self.copy_staging_bytes()
    }

    /// Compose the preview image: every visible layer in z-order, with each
    /// affected layer replaced by its filtered+mask-mixed scratch.
    pub fn render_filter_preview(&mut self, visibilities: &[bool]) -> Result<(), RendererError> {
        let spec = self.filter_spec;
        let visible_indices: Vec<usize> = visibilities
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| (v && i < self.layer_stack.slots.len()).then_some(i))
            .collect();
        let affected = self.filter_affected.clone();

        self.record_and_submit(|this| {
            this.cmd_clear_image(this.preview.handle, [0.0, 0.0, 0.0, 0.0]);
            Ok(())
        })?;

        for idx in visible_indices {
            if affected.contains(&idx) {
                let result = self.produce_filtered_layer(idx, spec)?;
                self.composite_scratch_into_preview(result, idx)?;
            } else {
                self.composite_layer_into_preview(idx)?;
            }
        }
        Ok(())
    }

    /// Render the filter preview and read it back as BGRA8. Test/diagnostic
    /// helper - the live path presents the preview image to the display
    /// rather than reading it to host memory.
    pub fn read_filter_preview(
        &mut self,
        visibilities: &[bool],
    ) -> Result<Vec<u8>, RendererError> {
        self.render_filter_preview(visibilities)?;
        let image = self.preview.handle;
        let extent = self.canvas.extent;
        self.read_image_to_staging(image, extent)?;
        self.copy_staging_bytes()
    }

    /// Apply the filter permanently to one layer: run the chain and copy the
    /// result into the layer image. Caller re-composites the canvas.
    pub fn apply_filter_to_layer(
        &mut self,
        idx: usize,
        spec: FilterSpec,
    ) -> Result<(), RendererError> {
        if idx >= self.layer_stack.slots.len() {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        let result = self.produce_filtered_layer(idx, spec)?;
        let src = self.filter_resources.scratch_handle(result);
        let dst = self.layer_stack.slots[idx].image.handle;
        let extent = self.canvas.extent;
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
                .src_subresource(color_layers())
                .dst_subresource(color_layers())
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
        })?;
        self.layer_stack.touch(idx);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Internal pass chain
    // ------------------------------------------------------------------

    /// Run the filter's pass chain for layer `idx`, ending with a mask-mix
    /// pass that blends the filtered result over the original by the
    /// selection mask. Returns the scratch slot holding the final image.
    fn produce_filtered_layer(
        &mut self,
        idx: usize,
        spec: FilterSpec,
    ) -> Result<Scratch, RendererError> {
        let layer_view = self.layer_stack.slots[idx].image.view;
        let layer_img = self.layer_stack.slots[idx].image.handle;

        let pre = self.produce_filtered_passes(layer_view, layer_img, spec)?;

        // Mask-mix: filtered = pre, original = layer, mask = selection mask,
        // written into the opposite scratch slot.
        let dst = pre.other();
        let pre_view = self.filter_resources.scratch_view(pre);
        let pre_img = self.filter_resources.scratch_handle(pre);
        let mask_view = self.selection.mask.view;
        let mask_img = self.selection.mask.handle;
        let sel_active = f32::from(u8::from(self.selection_active));
        self.filter_pass3(
            self.filter_resources.mask_mix,
            dst,
            pre_view,
            pre_img,
            layer_view,
            layer_img,
            mask_view,
            mask_img,
            [sel_active, 0.0, 0.0, 0.0],
        )?;
        Ok(dst)
    }

    /// Run only the per-spec effect passes against an arbitrary source image
    /// (a layer's pixels for destructive filters, or the canvas accumulator
    /// for adjustment layers), ping-ponging the scratch slots. Returns the
    /// scratch slot holding the filtered (not yet mask-mixed) result. The
    /// source image is read but never written, so the caller can use it as
    /// the "original" in a following mask-mix.
    pub(super) fn produce_filtered_passes(
        &mut self,
        src_view: vk::ImageView,
        src_img: vk::Image,
        spec: FilterSpec,
    ) -> Result<Scratch, RendererError> {
        let layer_view = src_view;
        let layer_img = src_img;
        #[allow(clippy::cast_precision_loss)]
        let inv_w = 1.0 / self.canvas.extent.width as f32;
        #[allow(clippy::cast_precision_loss)]
        let inv_h = 1.0 / self.canvas.extent.height as f32;

        // The "pre" scratch holds the filtered (but not yet mask-mixed) image.
        let pre = match spec {
            FilterSpec::Hsv {
                hue_degrees,
                saturation,
                value,
            } => {
                let push = [hue_degrees.to_radians(), saturation, value, 0.0];
                self.filter_pass(self.filter_resources.hsv, Scratch::A, layer_view, layer_img, push)?;
                Scratch::A
            }
            FilterSpec::Invert => {
                self.filter_pass(
                    self.filter_resources.invert,
                    Scratch::A,
                    layer_view,
                    layer_img,
                    [0.0; 4],
                )?;
                Scratch::A
            }
            FilterSpec::BoxBlur { radius_x, radius_y } => {
                self.filter_pass(
                    self.filter_resources.box_blur,
                    Scratch::A,
                    layer_view,
                    layer_img,
                    [inv_w, 0.0, radius_x, 0.0],
                )?;
                let a_view = self.filter_resources.scratch_a.view;
                let a_img = self.filter_resources.scratch_a.handle;
                self.filter_pass(
                    self.filter_resources.box_blur,
                    Scratch::B,
                    a_view,
                    a_img,
                    [0.0, inv_h, radius_y, 0.0],
                )?;
                Scratch::B
            }
            FilterSpec::Sharpen { amount } => {
                let r = FilterSpec::SHARPEN_BLUR_RADIUS;
                self.filter_pass(
                    self.filter_resources.box_blur,
                    Scratch::A,
                    layer_view,
                    layer_img,
                    [inv_w, 0.0, r, 0.0],
                )?;
                let a_view = self.filter_resources.scratch_a.view;
                let a_img = self.filter_resources.scratch_a.handle;
                self.filter_pass(
                    self.filter_resources.box_blur,
                    Scratch::B,
                    a_view,
                    a_img,
                    [0.0, inv_h, r, 0.0],
                )?;
                // sharpen reads original (binding 0) + blurred-in-B (binding 1).
                let b_view = self.filter_resources.scratch_b.view;
                let b_img = self.filter_resources.scratch_b.handle;
                self.filter_pass2(
                    self.filter_resources.sharpen,
                    Scratch::A,
                    layer_view,
                    layer_img,
                    b_view,
                    b_img,
                    [amount, 0.0, 0.0, 0.0],
                )?;
                Scratch::A
            }
        };
        Ok(pre)
    }

    /// Single-source filter pass (binding 0 = source; bindings 1/2 padded).
    fn filter_pass(
        &mut self,
        pipeline: vk::Pipeline,
        target: Scratch,
        src_view: vk::ImageView,
        src_img: vk::Image,
        push: [f32; 4],
    ) -> Result<(), RendererError> {
        self.filter_pass3(
            pipeline, target, src_view, src_img, src_view, src_img, src_view, src_img, push,
        )
    }

    /// Two-source filter pass (bindings 0 and 1; binding 2 padded with #1).
    #[allow(clippy::too_many_arguments)]
    fn filter_pass2(
        &mut self,
        pipeline: vk::Pipeline,
        target: Scratch,
        view0: vk::ImageView,
        img0: vk::Image,
        view1: vk::ImageView,
        img1: vk::Image,
        push: [f32; 4],
    ) -> Result<(), RendererError> {
        self.filter_pass3(
            pipeline, target, view0, img0, view1, img1, view1, img1, push,
        )
    }

    /// Three-source filter pass. Binds all three samplers, barriers each
    /// distinct source image to ensure prior writes are visible, then runs
    /// the fullscreen pass into `target`'s framebuffer.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn filter_pass3(
        &mut self,
        pipeline: vk::Pipeline,
        target: Scratch,
        view0: vk::ImageView,
        img0: vk::Image,
        view1: vk::ImageView,
        img1: vk::Image,
        view2: vk::ImageView,
        img2: vk::Image,
        push: [f32; 4],
    ) -> Result<(), RendererError> {
        let set = self.filter_resources.input_set(0);
        self.filter_resources
            .write_input(&self.device, set, view0, view1, view2);
        let layout = self.filter_resources.pipeline_layout;
        let render_pass = self.canvas_target.render_pass;
        let framebuffer = self.filter_resources.framebuffer(target);
        // Blocking: this rewrites the shared input_set(0) per pass, so the pass
        // must finish before the next call overwrites it (the per-frame batched
        // adjustment path uses distinct ring sets instead).
        self.record_and_submit(|this| {
            this.cmd_filter_pass3(
                set, layout, pipeline, render_pass, framebuffer, img0, img1, img2, push,
            );
            Ok(())
        })
    }

    /// Record (no submit) a three-source fullscreen filter pass into the current
    /// command buffer. The descriptor `set` must already be written. Barriers
    /// each distinct source GENERAL->GENERAL so prior writes are visible. Lets
    /// several passes be batched into one submission (the adjustment preview).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn cmd_filter_pass3(
        &mut self,
        set: vk::DescriptorSet,
        layout: vk::PipelineLayout,
        pipeline: vk::Pipeline,
        render_pass: vk::RenderPass,
        framebuffer: vk::Framebuffer,
        img0: vk::Image,
        img1: vk::Image,
        img2: vk::Image,
        push: [f32; 4],
    ) {
        self.barrier(img0, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        if img1 != img0 {
            self.barrier(img1, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        }
        if img2 != img0 && img2 != img1 {
            self.barrier(img2, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        }
        self.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
        unsafe {
            self.device.cmd_bind_descriptor_sets(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                &[set],
                &[],
            );
            let push_bytes =
                std::slice::from_raw_parts(push.as_ptr().cast::<u8>(), std::mem::size_of_val(&push));
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

    /// Record (no submit) the full jump-flood stroke band into `Scratch::A`:
    /// seed the backdrop silhouette, flood the nearest-edge distance field with
    /// halving step sizes, then resolve the coloured band. Descriptor sets are
    /// taken from the input ring starting at `*cursor` (one per pass, advanced
    /// past those used), so the chain can share a submission with other passes.
    /// `push` is the 48-byte resolve push (color, params, texel); `thickness`
    /// drives how many flood passes the band radius needs. The caller
    /// OVER-composites `Scratch::A` onto the accumulator.
    pub(super) fn cmd_stroke_band(
        &self,
        backdrop_view: vk::ImageView,
        backdrop_img: vk::Image,
        mask_view: vk::ImageView,
        mask_img: vk::Image,
        push: [f32; 12],
        thickness: f32,
        cursor: &mut usize,
    ) {
        let texel = [push[8], push[9]];
        let jfa_rp = self.filter_resources.jfa_render_pass;
        let layout16 = self.filter_resources.pipeline_layout;

        // Seed: classify the backdrop silhouette into coord A.
        let seed_set = self.filter_resources.input_set(*cursor);
        *cursor += 1;
        self.filter_resources
            .write_input(&self.device, seed_set, backdrop_view, backdrop_view, backdrop_view);
        self.barrier(backdrop_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        let seed_fb = self.filter_resources.coord_framebuffer(JfaSlot::A);
        let seed_pipe = self.filter_resources.jfa_seed;
        self.cmd_fullscreen_draw(seed_set, layout16, seed_pipe, jfa_rp, seed_fb, &[
            texel[0], texel[1], 0.0, 0.0,
        ]);

        // Flood: halving steps, ping-ponging the coord buffers.
        let mut src = JfaSlot::A;
        for step in jfa_step_sizes(thickness) {
            let dst = src.other();
            let set = self.filter_resources.input_set(*cursor);
            *cursor += 1;
            let src_view = self.filter_resources.coord_view(src);
            let src_img = self.filter_resources.coord_handle(src);
            self.filter_resources
                .write_input(&self.device, set, src_view, src_view, src_view);
            self.barrier(src_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            let dst_fb = self.filter_resources.coord_framebuffer(dst);
            let flood_pipe = self.filter_resources.jfa_flood;
            #[allow(clippy::cast_precision_loss)]
            let step_f = step as f32;
            self.cmd_fullscreen_draw(set, layout16, flood_pipe, jfa_rp, dst_fb, &[
                texel[0], texel[1], step_f, 0.0,
            ]);
            src = dst;
        }

        // Resolve: colour the band from the converged field into Scratch::A.
        let resolve_set = self.filter_resources.input_set(*cursor);
        *cursor += 1;
        let coord_view = self.filter_resources.coord_view(src);
        let coord_img = self.filter_resources.coord_handle(src);
        self.filter_resources
            .write_input(&self.device, resolve_set, coord_view, backdrop_view, mask_view);
        self.barrier(coord_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        self.barrier(backdrop_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        if mask_img != backdrop_img && mask_img != coord_img {
            self.barrier(mask_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        }
        let resolve_fb = self.filter_resources.framebuffer(Scratch::A);
        let resolve_pipe = self.filter_resources.jfa_resolve;
        let resolve_layout = self.filter_resources.stroke_layout;
        let render_pass = self.canvas_target.render_pass;
        self.cmd_fullscreen_draw(resolve_set, resolve_layout, resolve_pipe, render_pass, resolve_fb, &push);
    }

    /// Begin a fullscreen pass, bind `set`, push `push` (its byte length, so a
    /// 16- or 48-byte push both work), draw the fullscreen triangle, end. The
    /// caller barriers the sources first.
    fn cmd_fullscreen_draw(
        &self,
        set: vk::DescriptorSet,
        layout: vk::PipelineLayout,
        pipeline: vk::Pipeline,
        render_pass: vk::RenderPass,
        framebuffer: vk::Framebuffer,
        push: &[f32],
    ) {
        self.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
        unsafe {
            self.device.cmd_bind_descriptor_sets(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                &[set],
                &[],
            );
            let push_bytes =
                std::slice::from_raw_parts(push.as_ptr().cast::<u8>(), std::mem::size_of_val(push));
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

    /// Blend one plain layer image into the preview framebuffer at the layer's
    /// own blend mode + opacity.
    fn composite_layer_into_preview(&mut self, idx: usize) -> Result<(), RendererError> {
        let descriptor_set = self.layer_stack.slots[idx].descriptor_set;
        let (mode, opacity) = self.layer_stack.blend(idx);
        self.composite_set_into_preview(descriptor_set, mode, opacity)
    }

    /// Blend a finished scratch image (an affected layer's filtered result)
    /// into the preview at the source layer's blend mode + opacity.
    fn composite_scratch_into_preview(
        &mut self,
        which: Scratch,
        idx: usize,
    ) -> Result<(), RendererError> {
        let src_img = self.filter_resources.scratch_handle(which);
        let descriptor_set = self.filter_resources.composite_set(which);
        self.record_and_submit(|this| {
            this.barrier(src_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            Ok(())
        })?;
        let (mode, opacity) = self.layer_stack.blend(idx);
        self.composite_set_into_preview(descriptor_set, mode, opacity)
    }

    /// Shared body: blend whatever `descriptor_set` binds (a layer or a scratch
    /// image) into the preview using the layer-blend pipeline.
    fn composite_set_into_preview(
        &mut self,
        descriptor_set: vk::DescriptorSet,
        mode: u32,
        opacity: f32,
    ) -> Result<(), RendererError> {
        let preview_img = self.preview.handle;
        let preview_fb = self.preview_framebuffer;
        self.record_and_submit(|this| {
            this.cmd_compose_layer_blended(preview_img, preview_fb, descriptor_set, mode, opacity);
            Ok(())
        })
    }
}

const fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// Jump-flood step sizes for a stroke of `thickness` pixels: the smallest power
/// of two covering the band radius, halving down to 1, plus one extra step-1
/// pass (JFA+1) to clean up the rare propagation error. The number of passes is
/// `O(log thickness)`, where the old disc scan was `O(thickness^2)` per pixel.
fn jfa_step_sizes(thickness: f32) -> Vec<u32> {
    // Seeds must reach the band's far edge (~thickness) plus an AA pixel.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let radius = (thickness.ceil() as u32).saturating_add(2).clamp(1, 64);
    let mut start = 1u32;
    while start < radius {
        start <<= 1;
    }
    let mut steps = Vec::new();
    let mut step = start;
    loop {
        steps.push(step);
        if step == 1 {
            break;
        }
        step >>= 1;
    }
    steps.push(1);
    steps
}
