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

use crate::filters::FilterSpec;

use super::super::RendererError;
use super::super::filters::Scratch;
use super::VulkanRenderer;

impl VulkanRenderer {
    /// Arm the filter preview path for `affected` layers with `spec`.
    pub fn begin_filter(&mut self, affected: Vec<usize>, spec: FilterSpec) {
        self.filter_active = true;
        self.filter_affected = affected;
        self.filter_spec = spec;
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
                self.composite_scratch_into_preview(result)?;
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

        // Mask-mix: filtered = pre, original = layer, mask = selection mask,
        // written into the opposite scratch slot.
        let dst = match pre {
            Scratch::A => Scratch::B,
            Scratch::B => Scratch::A,
        };
        let pre_view = match pre {
            Scratch::A => self.filter_resources.scratch_a.view,
            Scratch::B => self.filter_resources.scratch_b.view,
        };
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
    fn filter_pass3(
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
        self.filter_resources
            .write_input(&self.device, view0, view1, view2);
        let layout = self.filter_resources.pipeline_layout;
        let set = self.filter_resources.input_set;
        let render_pass = self.canvas_target.render_pass;
        let framebuffer = self.filter_resources.framebuffer(target);

        self.record_and_submit(|this| {
            // Make prior writes to the source images visible to the sampler.
            this.barrier(img0, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            if img1 != img0 {
                this.barrier(img1, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            }
            if img2 != img0 && img2 != img1 {
                this.barrier(img2, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            }
            this.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
            unsafe {
                this.device.cmd_bind_descriptor_sets(
                    this.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &[set],
                    &[],
                );
                let push_bytes = std::slice::from_raw_parts(
                    push.as_ptr().cast::<u8>(),
                    std::mem::size_of_val(&push),
                );
                this.device.cmd_push_constants(
                    this.command_buffer,
                    layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    push_bytes,
                );
            }
            this.cmd_end_fullscreen_pass();
            Ok(())
        })
    }

    /// OVER-blend one plain layer image into the preview framebuffer.
    fn composite_layer_into_preview(&mut self, idx: usize) -> Result<(), RendererError> {
        let descriptor_set = self.layer_stack.slots[idx].descriptor_set;
        self.composite_set_into_preview(descriptor_set)
    }

    /// OVER-blend a finished scratch image into the preview framebuffer.
    fn composite_scratch_into_preview(&mut self, which: Scratch) -> Result<(), RendererError> {
        let src_img = self.filter_resources.scratch_handle(which);
        let descriptor_set = self.filter_resources.composite_set(which);
        self.record_and_submit(|this| {
            this.barrier(src_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            Ok(())
        })?;
        self.composite_set_into_preview(descriptor_set)
    }

    /// Shared body: OVER-blend whatever `descriptor_set` binds (a layer or a
    /// scratch image) into the preview using the layer-composite pipeline.
    fn composite_set_into_preview(
        &mut self,
        descriptor_set: vk::DescriptorSet,
    ) -> Result<(), RendererError> {
        let render_pass = self.canvas_target.render_pass;
        let framebuffer = self.preview_framebuffer;
        let pipeline = self.layer_composite_pipeline.pipeline;
        let layout = self.layer_composite_pipeline.layout;
        let preview_img = self.preview.handle;
        self.record_and_submit(|this| {
            // Each composite is its own submission, so make the prior layer's
            // OVER-write to the preview visible to this pass's LOAD.
            this.barrier(preview_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
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
}

const fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    }
}
