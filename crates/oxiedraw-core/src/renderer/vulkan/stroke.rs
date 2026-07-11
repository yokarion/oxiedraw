//! Per-stroke ops: stamping dabs, clearing the stroke buffer, compositing
//! the stroke onto the canvas.

use ash::vk;

use super::super::RendererError;
use super::super::dab::{DabFamily, DabInstance, DabPushConstants};
use super::VulkanRenderer;

impl VulkanRenderer {
    /// Rasterize `dabs` onto the canvas with the brush dab pipeline
    /// (premultiplied-alpha OVER blend). Submits and waits.
    pub fn paint_dabs(
        &mut self,
        family: DabFamily,
        dabs: &[DabInstance],
    ) -> Result<(), RendererError> {
        if dabs.is_empty() {
            return Ok(());
        }
        let slot = self.current_ring_slot();
        self.wait_ring_slot(slot)?;
        let n = self.dab_buffers.upload_instances(dabs, slot)?;
        let pipe = self.dab_pipelines.get(family);
        let pipeline = pipe.pipeline;
        let layout = pipe.layout;
        let render_pass = self.canvas_target.render_pass;
        let framebuffer = self.canvas_target.framebuffer;
        self.record_dab_pass(family, pipeline, layout, render_pass, framebuffer, n)
    }

    /// Reset the accumulated stroke dirty-rect. Call at the start of a
    /// stroke so the history patch only covers this stroke's dabs.
    pub fn reset_stroke_dirty(&mut self) {
        self.stroke_dirty = None;
    }

    /// Set whether the in-flight stroke erases (removes target-layer
    /// coverage) instead of painting. Call at the start of a stroke.
    pub fn set_stroke_erase(&mut self, erase: bool) {
        self.stroke_erase = erase;
    }

    /// Set whether the in-flight stroke accumulates coverage (build-up,
    /// OVER blend) instead of saturating (MAX). Call at the start of a
    /// stroke. Reset to false by `begin_stroke` for ordinary brushes.
    pub fn set_stroke_buildup(&mut self, buildup: bool) {
        self.stroke_buildup = buildup;
    }

    /// The mask pipeline set the current stroke stamps with: OVER-blend
    /// for build-up, MAX-blend otherwise.
    pub(super) fn active_mask_pipelines(&self) -> &super::super::mask::MaskPipelineSet {
        if self.stroke_buildup {
            &self.mask_pipelines_buildup
        } else {
            &self.mask_pipelines
        }
    }

    /// The tight integer AABB `(x, y, w, h)` of everything stamped since
    /// the last reset, clamped to the canvas. `None` if nothing was
    /// stamped or the rect is empty after clamping.
    #[must_use]
    pub fn stroke_dirty_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        let (min_x, min_y, max_x, max_y) = self.stroke_dirty?;
        #[allow(clippy::cast_precision_loss)]
        let cw = self.canvas_size.width as f32;
        #[allow(clippy::cast_precision_loss)]
        let ch = self.canvas_size.height as f32;
        let x0 = min_x.floor().clamp(0.0, cw);
        let y0 = min_y.floor().clamp(0.0, ch);
        let x1 = max_x.ceil().clamp(0.0, cw);
        let y1 = max_y.ceil().clamp(0.0, ch);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some((x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
    }

    /// Union each dab's quad AABB into the accumulated dirty-rect. The
    /// dab vertex shaders expand the unit quad by `radius` (soft-round /
    /// pixel) or by `radius * (squish, rotate)` (textured), so a quad
    /// half-extent of `radius * sqrt(1 + aspect^2)` is a guaranteed
    /// superset of any rotated/squished dab. `+1` pads for AA fringe.
    pub(super) fn accumulate_dirty(&mut self, dabs: &[DabInstance]) {
        for d in dabs {
            let half = d.radius * (1.0 + d.aspect * d.aspect).sqrt() + 1.0;
            let (cx, cy) = (d.center[0], d.center[1]);
            let (nx0, ny0, nx1, ny1) = (cx - half, cy - half, cx + half, cy + half);
            let grow = |dirty: Option<(f32, f32, f32, f32)>| {
                Some(match dirty {
                    Some((x0, y0, x1, y1)) => {
                        (x0.min(nx0), y0.min(ny0), x1.max(nx1), y1.max(ny1))
                    }
                    None => (nx0, ny0, nx1, ny1),
                })
            };
            // Cumulative (for the history patch) and per-preview-frame (for the
            // incremental preview, reset by each preview build).
            self.stroke_dirty = grow(self.stroke_dirty);
            self.preview_pending_dirty = grow(self.preview_pending_dirty);
        }
    }

    /// Stamp `dabs` into the stroke buffer with MAX blending.
    pub fn stamp_mask(
        &mut self,
        family: DabFamily,
        dabs: &[DabInstance],
    ) -> Result<(), RendererError> {
        if dabs.is_empty() {
            return Ok(());
        }
        self.accumulate_dirty(dabs);
        let slot = self.current_ring_slot();
        self.wait_ring_slot(slot)?;
        let n = self.dab_buffers.upload_instances(dabs, slot)?;
        let pipe = self.active_mask_pipelines().get(family);
        let pipeline = pipe.pipeline;
        let layout = pipe.layout;
        let render_pass = self.stroke_target.render_pass;
        let framebuffer = self.stroke_target.framebuffer;
        self.record_dab_pass(family, pipeline, layout, render_pass, framebuffer, n)
    }

    /// Clear the stroke buffer to fully-transparent.
    pub fn clear_stroke(&mut self) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            this.cmd_clear_image(this.stroke.handle, [0.0, 0.0, 0.0, 0.0]);
            Ok(())
        })
    }

    /// Composite the stroke buffer onto the canvas at `color_linear`
    /// tint and `opacity`. Premultiplied OVER blend.
    pub fn composite_stroke(
        &mut self,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<(), RendererError> {
        let push: [f32; 4] = [
            color_linear[0],
            color_linear[1],
            color_linear[2],
            opacity.clamp(0.0, 1.0),
        ];
        let framebuffer = self.canvas_target.framebuffer;
        self.record_composite_pass(framebuffer, push)
    }

    pub(super) fn record_dab_pass(
        &mut self,
        family: DabFamily,
        pipeline: vk::Pipeline,
        layout: vk::PipelineLayout,
        render_pass: vk::RenderPass,
        framebuffer: vk::Framebuffer,
        instance_count: u32,
    ) -> Result<(), RendererError> {
        // Async: the brush dab is the hot input path. The dab pass binds fixed
        // resources, so submission order keeps the following preview correct.
        self.record_and_submit_async(|this| {
            this.cmd_dab_pass(family, pipeline, layout, render_pass, framebuffer, instance_count);
            Ok(())
        })
    }

    /// Record (no submit) the instanced dab draw into `framebuffer`.
    /// Caller wraps in `record_and_submit` and must have uploaded the
    /// `instance_count` instances via `dab_buffers.upload_instances` first.
    pub(super) fn cmd_dab_pass(
        &self,
        family: DabFamily,
        pipeline: vk::Pipeline,
        layout: vk::PipelineLayout,
        render_pass: vk::RenderPass,
        framebuffer: vk::Framebuffer,
        instance_count: u32,
    ) {
        let inv_size = self.canvas_inv_size();
        let push = DabPushConstants {
            inv_size,
            slice: family.slice(),
        };
        let atlas_set = self.pattern_atlas.descriptor_set();
        let binds_atlas = family.binds_atlas();
        self.cmd_begin_fullscreen_pass(render_pass, framebuffer, pipeline);
        unsafe {
            self.device.cmd_push_constants(
                self.command_buffer,
                layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push.as_bytes(),
            );
            if binds_atlas {
                self.device.cmd_bind_descriptor_sets(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &[atlas_set],
                    &[],
                );
            }
            self.device.cmd_bind_vertex_buffers(
                self.command_buffer,
                0,
                &[self.dab_buffers.vertex.handle],
                &[0],
            );
            // Bind this slot's instance region. ring_cursor still points at the
            // slot being recorded, which matches the slot the upload wrote to.
            self.device.cmd_bind_vertex_buffers(
                self.command_buffer,
                1,
                &[self.dab_buffers.instance.handle],
                &[super::super::dab::instance_slot_offset(self.ring_cursor)],
            );
            self.device
                .cmd_draw(self.command_buffer, 4, instance_count, 0, 0);
            self.device.cmd_end_render_pass(self.command_buffer);
        }
    }
}
