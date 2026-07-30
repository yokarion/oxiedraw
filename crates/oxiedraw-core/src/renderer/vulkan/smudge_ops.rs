//! Colour-smudge dab path (Krita colorsmudge, smearing mode).
//!
//! Unlike the mask brushes - which accumulate an R8 coverage mask in the
//! stroke buffer and composite it with a single tint at commit - a smudge
//! dab deposits a *sampled* colour that varies along the stroke. Each dab is
//! painted straight into the target layer: the layer is copied into
//! `blend_scratch` before a batch of dabs, and every dab samples that copy at
//! the drag-shifted position (dragging the colour under the previous dab onto
//! the current one) and composites it OVER the layer. Fully GPU - no readback.

use ash::vk;
use std::mem::ManuallyDrop;

use super::super::RendererError;
use super::super::pass::{FullscreenPass, over_blend};
use super::super::resources::Image;
use super::{CANVAS_FORMAT, VulkanRenderer, create_sampled_image_set};

const COMPOSITE_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/composite.vert.spv"));
const DAB_SMUDGE_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dab_smudge.frag.spv"));

/// One smudge dab handed to [`VulkanRenderer::smudge_dabs`]. All lengths are
/// canvas pixels; `delta` is `center - previous_center` (the drag vector).
#[derive(Debug, Clone, Copy)]
pub struct SmudgeDab {
    pub center: [f32; 2],
    pub delta: [f32; 2],
    pub radius: f32,
    pub hardness: f32,
    pub smudge_rate: f32,
    pub color_rate: f32,
}

/// Push constants for `dab_smudge.frag`. Layout must match the shader.
#[repr(C)]
#[derive(Clone, Copy)]
struct SmudgePush {
    paint: [f32; 4],
    center: [f32; 2],
    delta: [f32; 2],
    inv_size: [f32; 2],
    radius: f32,
    hardness: f32,
    smudge_rate: f32,
    color_rate: f32,
    opacity: f32,
}

impl SmudgePush {
    const SIZE: u32 = 60;

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: repr(C) POD of f32s.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

impl VulkanRenderer {
    /// Build the smudge pipeline on first use (most sessions never smudge).
    /// Two descriptor sets, both the layer-composite single-sampler layout:
    /// set 0 = `blend_scratch` (per-dab layer copy), set 1 = `smudge_before`.
    fn ensure_smudge_pipeline(&mut self) -> Result<(vk::PipelineLayout, vk::Pipeline), RendererError> {
        if let Some(pair) = self.smudge_pipeline {
            return Ok(pair);
        }
        let set_layout = self.layer_composite_pipeline.descriptor_set_layout;
        let set_layouts = [set_layout, set_layout];
        let push_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(SmudgePush::SIZE)];
        let info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_ranges);
        let layout = unsafe { self.device.create_pipeline_layout(&info, None)? };
        let pipeline = FullscreenPass {
            vert_spv: COMPOSITE_VERT_SPV,
            frag_spv: DAB_SMUDGE_FRAG_SPV,
            render_pass: self.canvas_target.render_pass,
            layout,
            blend: over_blend(),
        }
        .build(&self.device)?;
        self.smudge_pipeline = Some((layout, pipeline));
        Ok((layout, pipeline))
    }

    /// Ensure the pre-stroke snapshot image exists (lazy) and return its
    /// sampler set. Allocated at canvas size, primed to GENERAL.
    fn ensure_smudge_before(&mut self) -> Result<vk::DescriptorSet, RendererError> {
        if let Some((_, _, set)) = self.smudge_before {
            return Ok(set);
        }
        let extent = self.canvas_extent_2d();
        let image = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "smudge-before",
            CANVAS_FORMAT,
            extent,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            vk::ImageAspectFlags::COLOR,
        )?;
        let (pool, set) = create_sampled_image_set(
            &self.device,
            self.layer_composite_pipeline.descriptor_set_layout,
            self.layer_composite_pipeline.sampler,
            image.view,
        )?;
        let handle = image.handle;
        self.smudge_before = Some((ManuallyDrop::new(image), pool, set));
        // Prime UNDEFINED -> GENERAL so the first copy-into is well defined.
        self.record_and_submit(|this| {
            this.barrier(handle, vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL);
            Ok(())
        })?;
        Ok(set)
    }

    /// Snapshot layer `layer_idx` into `smudge_before` at stroke start, so the
    /// dab shader can lerp deposits from it (opacity ceiling). No-op if the
    /// index is out of range.
    pub fn begin_smudge_stroke(&mut self, layer_idx: usize) -> Result<(), RendererError> {
        if layer_idx >= self.layer_stack.slots.len() {
            return Ok(());
        }
        self.ensure_smudge_before()?;
        let layer_img = self.layer_stack.slots[layer_idx].image.handle;
        let before_img = self.smudge_before.as_ref().expect("ensured above").0.handle;
        self.record_and_submit(|this| {
            this.clip = None;
            this.cmd_copy_image_full(layer_img, before_img);
            Ok(())
        })
    }

    /// Read a region of the pre-stroke `smudge_before` snapshot back to CPU
    /// (BGRA8, row-major). Used by undo: the smudge layer is mutated live, so
    /// the pristine before-state is read from this snapshot at pen-up over just
    /// the dirty rect, avoiding a full-canvas readback at pen-down. Returns an
    /// empty `out` if no snapshot exists or the region is empty.
    pub fn read_smudge_before_region_into(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), RendererError> {
        let Some(image) = self.smudge_before.as_ref().map(|(img, _, _)| img.handle) else {
            out.clear();
            return Ok(());
        };
        if w == 0 || h == 0 {
            out.clear();
            return Ok(());
        }
        self.read_image_region_to_staging(image, x, y, w, h)?;
        let len = (w as usize) * (h as usize) * 4;
        let bytes = self.staging.mapped().ok_or(RendererError::StagingNotMapped)?;
        out.clear();
        out.extend_from_slice(&bytes[..len]);
        Ok(())
    }

    /// Paint a batch of smudge dabs into layer `layer_idx`. `paint_linear` is
    /// the premultiplied-linear brush colour (RGB + alpha 1). The whole batch
    /// samples one pre-batch copy of the layer, so dabs within a batch smear
    /// from the same source; state carries across batches through the layer
    /// itself (each new batch re-copies it).
    pub fn smudge_dabs(
        &mut self,
        layer_idx: usize,
        paint_linear: [f32; 4],
        opacity: f32,
        dabs: &[SmudgeDab],
    ) -> Result<(), RendererError> {
        if layer_idx >= self.layer_stack.slots.len() || dabs.is_empty() {
            return Ok(());
        }
        self.ensure_smudge_pipeline()?;
        self.ensure_smudge_before()?;
        let dabs = dabs.to_vec();
        self.record_and_submit(|this| {
            this.cmd_smudge_dabs(layer_idx, paint_linear, opacity, &dabs);
            Ok(())
        })?;
        self.finish_smudge_dabs(layer_idx, &dabs);
        Ok(())
    }

    /// Paint a batch of smudge dabs, rebuild the canvas from the layer stack,
    /// and present - all in ONE async submit (no CPU fence stall), mirroring
    /// the normal brush's `stamp_preview_present`. `visible_indices` and
    /// `present_clip` come from the canvas. Falls back to nothing if there are
    /// no dabs. Caller reads `display_descriptor()` afterwards.
    pub fn smudge_stamp_present(
        &mut self,
        layer_idx: usize,
        paint_linear: [f32; 4],
        opacity: f32,
        dabs: &[SmudgeDab],
        visibilities: &[bool],
    ) -> Result<(), RendererError> {
        if layer_idx >= self.layer_stack.slots.len() || dabs.is_empty() {
            return Ok(());
        }
        self.ensure_smudge_pipeline()?;
        self.ensure_smudge_before()?;
        let dabs = dabs.to_vec();
        let visible = self.visible_layer_indices(visibilities);
        let present_clip = present_scissor(&dabs, self.canvas_size);
        let canvas_img = self.canvas.handle;
        let canvas_view = self.canvas.view;
        self.record_and_submit_async(|this| {
            this.cmd_frame_timing_begin();
            this.cmd_smudge_dabs(layer_idx, paint_linear, opacity, &dabs);
            // Rebuild the canvas from the layer stack (the smudged layer changed).
            this.cmd_composite_layers_to_canvas(&visible);
            this.cmd_frame_timing_mark(1);
            // Present only the stroke's dirty region.
            this.clip = present_clip;
            this.record_present_copy(canvas_img, canvas_view);
            this.clip = None;
            this.cmd_frame_timing_mark(2);
            Ok(())
        })?;
        self.finish_smudge_dabs(layer_idx, &dabs);
        self.note_frame_timing();
        Ok(())
    }

    /// Record (no submit) every dab of a batch into the current command buffer.
    /// Per dab, copy just its neighbourhood (write disc + drag-back read disc)
    /// of the layer into the scratch, then deposit sampling that copy - so every
    /// dab reads the layer *including the previous dab's deposit* (continuous
    /// smear, no banding), with a barrier between dabs ordering the writes. The
    /// deposit is anchored to the pre-stroke `smudge_before` snapshot so opacity
    /// is a ceiling and overlapping dabs converge smoothly. Assumes
    /// `ensure_smudge_pipeline` / `ensure_smudge_before` already ran.
    fn cmd_smudge_dabs(
        &mut self,
        layer_idx: usize,
        paint_linear: [f32; 4],
        opacity: f32,
        dabs: &[SmudgeDab],
    ) {
        let (layout, pipeline) = self.smudge_pipeline.expect("pipeline ensured");
        let before_set = self.smudge_before.as_ref().expect("before ensured").2;
        let layer_fb = self.layer_stack.slots[layer_idx].framebuffer;
        let layer_img = self.layer_stack.slots[layer_idx].image.handle;
        let scratch_img = self.blend_scratch.handle;
        let scratch_set = self.blend_scratch_dst_set;
        let render_pass = self.canvas_target.render_pass;
        let size = self.canvas_size;
        #[allow(clippy::cast_precision_loss)]
        let inv_size = [1.0 / size.width as f32, 1.0 / size.height as f32];
        for d in dabs {
            // Skip dabs whose copy/render rect collapses to nothing (e.g. a
            // centre dragged off-canvas): a zero-extent copy region / render
            // area is an invalid Vulkan command.
            let Some(scissor) = smudge_copy_scissor(d.center, d.delta, d.radius, size) else {
                continue;
            };
            self.clip = Some(scissor);
            self.cmd_copy_image_full(layer_img, scratch_img);
            let push = SmudgePush {
                paint: paint_linear,
                center: d.center,
                delta: d.delta,
                inv_size,
                radius: d.radius,
                hardness: d.hardness,
                smudge_rate: d.smudge_rate,
                color_rate: d.color_rate,
                opacity,
            };
            self.cmd_begin_fullscreen_pass(render_pass, layer_fb, pipeline);
            unsafe {
                self.device.cmd_bind_descriptor_sets(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &[scratch_set, before_set],
                    &[],
                );
                self.device.cmd_push_constants(
                    self.command_buffer,
                    layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    push.as_bytes(),
                );
            }
            self.cmd_end_fullscreen_pass();
            // Order this dab's layer write before the next dab reads it.
            self.barrier(layer_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
        }
        self.clip = None;
    }

    /// Post-submit bookkeeping shared by both smudge paths.
    fn finish_smudge_dabs(&mut self, layer_idx: usize, dabs: &[SmudgeDab]) {
        self.layer_stack.touch(layer_idx);
        for d in dabs {
            self.accumulate_smudge_dirty(d.center, d.radius);
        }
    }

    /// Grow the stroke dirty-rects to include a dab (for the history patch and
    /// the incremental present). Mirrors `accumulate_dirty` for a single disc.
    fn accumulate_smudge_dirty(&mut self, center: [f32; 2], radius: f32) {
        let half = radius + 1.0;
        let (nx0, ny0, nx1, ny1) = (
            center[0] - half,
            center[1] - half,
            center[0] + half,
            center[1] + half,
        );
        let grow = |dirty: Option<(f32, f32, f32, f32)>| {
            Some(match dirty {
                Some((x0, y0, x1, y1)) => (x0.min(nx0), y0.min(ny0), x1.max(nx1), y1.max(ny1)),
                None => (nx0, ny0, nx1, ny1),
            })
        };
        self.stroke_dirty = grow(self.stroke_dirty);
        self.preview_pending_dirty = grow(self.preview_pending_dirty);
    }
}

/// Present clip: the union of the dabs' write discs (centre +/- radius),
/// clamped to the canvas - the region the canvas actually changed this event,
/// so the dmabuf present only refreshes that area. `None` if empty.
fn present_scissor(dabs: &[SmudgeDab], size: oxiedraw_utils::geometry::Size) -> Option<vk::Rect2D> {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for d in dabs {
        let h = d.radius + 2.0;
        x0 = x0.min(d.center[0] - h);
        y0 = y0.min(d.center[1] - h);
        x1 = x1.max(d.center[0] + h);
        y1 = y1.max(d.center[1] + h);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        let ix0 = x0.floor().max(0.0) as i32;
        let iy0 = y0.floor().max(0.0) as i32;
        let ix1 = x1.ceil().min(size.width as f32) as i32;
        let iy1 = y1.ceil().min(size.height as f32) as i32;
        if ix1 <= ix0 || iy1 <= iy0 {
            return None;
        }
        Some(vk::Rect2D {
            offset: vk::Offset2D { x: ix0, y: iy0 },
            extent: vk::Extent2D {
                width: (ix1 - ix0) as u32,
                height: (iy1 - iy0) as u32,
            },
        })
    }
}

/// Scissor / copy rect covering both a dab's write disc (centre +/- radius)
/// and its drag-back read disc (centre - delta +/- radius), clamped to the
/// canvas. Used for both the per-dab region copy and the dab render pass.
/// `None` when the clamped rect is empty (the dab lies fully off-canvas), so
/// the caller can skip it rather than issue a zero-extent Vulkan command.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn smudge_copy_scissor(
    center: [f32; 2],
    delta: [f32; 2],
    radius: f32,
    size: oxiedraw_utils::geometry::Size,
) -> Option<vk::Rect2D> {
    let half = radius + 2.0;
    let read = [center[0] - delta[0], center[1] - delta[1]];
    let x0 = (center[0].min(read[0]) - half).floor().max(0.0) as i32;
    let y0 = (center[1].min(read[1]) - half).floor().max(0.0) as i32;
    let x1 = (center[0].max(read[0]) + half).ceil().min(size.width as f32) as i32;
    let y1 = (center[1].max(read[1]) + half).ceil().min(size.height as f32) as i32;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
    })
}
