//! Adjustment-layer compositing: non-destructive effects applied to the
//! canvas accumulator (everything below the layer) at composite time.
//!
//! When any slot is an adjustment layer the canvas can no longer be built in a
//! single command submission, because each effect runs its own multi-pass
//! filter chain (each pass is a separate submit with barriers). This module
//! provides the per-layer-submit composite loop that the renderer falls back
//! to in that case; the fast single-submit path stays for the common case of
//! no adjustment layers.

use ash::vk;

use super::super::RendererError;
use super::{create_framebuffer_for_view, create_sampled_image_set, VulkanRenderer, CANVAS_FORMAT};
use crate::renderer::resources::Image;
use crate::document::CompositeStep;
use crate::effects::{AdjustmentData, EffectKind};
use crate::filters::FilterSpec;
use crate::renderer::PresentSource;
use crate::renderer::filters::INPUT_RING;

/// A canvas-sized sub-accumulator for one folder nesting level: an image to
/// composite the folder's contents into, plus the descriptor set that samples
/// it when the finished folder is blended onto its parent.
pub(in crate::renderer) struct GroupAccumulator {
    pub(super) image: std::mem::ManuallyDrop<Image>,
    pub(super) framebuffer: vk::Framebuffer,
    descriptor_pool: vk::DescriptorPool,
    pub(super) descriptor_set: vk::DescriptorSet,
}

impl GroupAccumulator {
    /// Release the GPU resources. Caller must hold the device idle.
    pub(super) unsafe fn destroy(
        &mut self,
        device: &ash::Device,
        allocator: &mut gpu_allocator::vulkan::Allocator,
    ) {
        unsafe {
            device.destroy_framebuffer(self.framebuffer, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            std::mem::ManuallyDrop::take(&mut self.image).destroy(device, allocator);
        }
    }
}

/// The accumulator an adjustment chain reads + writes: the committed canvas
/// during a recomposite, or the preview image during a live stroke.
#[derive(Clone, Copy)]
struct Accumulator {
    image: vk::Image,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
}

/// One open scope while walking a scoped preview: the accumulator children
/// compose into, the descriptor set that samples it when it folds into its
/// parent, and (for a freshly rebuilt static folder) the cache slot its result
/// is copied to.
struct ScopeFrame {
    acc: Accumulator,
    fold_set: vk::DescriptorSet,
    cache_ordinal: Option<usize>,
    used_depth: bool,
}

/// Pre-order folder metadata for a scoped step stream.
#[derive(Clone, Copy)]
struct GroupSpan {
    /// Pre-order index of this folder among all folders (its cache key).
    ordinal: usize,
    /// Step index of the matching `ExitGroup`.
    exit: usize,
    /// Whether the stroke target layer composites inside this folder.
    contains_target: bool,
}

/// Index each `EnterGroup` step to its [`GroupSpan`]. A folder "contains" the
/// target when the target's `Layer` step falls between its `EnterGroup` and
/// `ExitGroup`. Folders are numbered in pre-order so the numbering is stable for
/// a fixed step stream (which holds for the duration of one stroke).
fn scoped_group_spans(steps: &[CompositeStep], target_idx: usize) -> Vec<Option<GroupSpan>> {
    let target_step = steps
        .iter()
        .position(|s| matches!(s, CompositeStep::Layer(i) if *i == target_idx));
    let mut spans = vec![None; steps.len()];
    let mut open: Vec<(usize, usize)> = Vec::new(); // (enter index, ordinal)
    let mut next_ordinal = 0usize;
    for (i, step) in steps.iter().enumerate() {
        match step {
            CompositeStep::EnterGroup => {
                open.push((i, next_ordinal));
                next_ordinal += 1;
            }
            CompositeStep::ExitGroup => {
                if let Some((enter, ordinal)) = open.pop() {
                    let contains_target = target_step.is_some_and(|t| t > enter && t < i);
                    spans[enter] = Some(GroupSpan {
                        ordinal,
                        exit: i,
                        contains_target,
                    });
                }
            }
            CompositeStep::Layer(_) => {}
        }
    }
    spans
}

/// How the live in-flight content is merged into the target layer when building
/// a folder-scoped preview: a brush stroke over the target, a warped copy of the
/// target (the transform tool), or the gradient ramp spliced over the target.
#[derive(Clone, Copy)]
pub(super) enum PreviewTarget {
    Stroke { push: [f32; 4], erase: bool },
    Warp { set: vk::DescriptorSet, mode: u32, opacity: f32, visible: bool },
    Gradient { endpoints: [f32; 4], extra: [f32; 4] },
    // The target layer's filtered result, pre-produced into a filter scratch.
    Filter { src_img: vk::Image, set: vk::DescriptorSet, mode: u32, opacity: f32 },
}

/// The in-flight mask stroke for a live mask-edit preview: which adjustment slot
/// is being painted and the brush push (linear color + opacity) / erase flag the
/// stroke is merged with.
#[derive(Clone, Copy)]
pub(in crate::renderer) struct MaskEditPreview {
    pub(super) target_idx: usize,
    pub(super) push: [f32; 4],
    pub(super) erase: bool,
}

/// Map an adjustment effect to the destructive [`FilterSpec`] pass chain it
/// reuses. `Stroke` has no `FilterSpec` equivalent (it is a separate SDF pass)
/// and returns `None`.
fn effect_to_filter_spec(kind: EffectKind) -> Option<FilterSpec> {
    match kind {
        EffectKind::HueSatBright {
            hue_degrees,
            saturation,
            brightness,
        } => Some(FilterSpec::Hsv {
            hue_degrees,
            saturation,
            value: brightness,
        }),
        EffectKind::Blur { radius_x, radius_y } => Some(FilterSpec::BoxBlur { radius_x, radius_y }),
        EffectKind::Invert => Some(FilterSpec::Invert),
        EffectKind::Sharpen { amount } => Some(FilterSpec::Sharpen { amount }),
        EffectKind::Stroke { .. } => None,
    }
}

impl VulkanRenderer {
    /// Set or clear the adjustment effect stack on slot `idx`. Pure GPU-side
    /// metadata (the effect parameters); the caller re-composites the canvas.
    pub fn set_layer_adjustment(&mut self, idx: usize, data: Option<AdjustmentData>) {
        self.layer_stack.set_adjustment(idx, data);
        self.preview_cache_valid = false;
        self.scoped_cache_valid = false;
    }

    /// `true` when at least one slot is an adjustment layer.
    #[must_use]
    pub fn has_adjustment_layers(&self) -> bool {
        self.layer_stack.has_adjustments()
    }

    /// Rebuild the canvas image bottom-up, applying each adjustment layer's
    /// effect chain to the accumulator as it is reached. Mirrors
    /// `cmd_composite_layers_to_canvas` but submits per layer so the effect
    /// passes can run.
    pub(super) fn composite_layers_to_canvas_adjusted(
        &mut self,
        visible_indices: &[usize],
    ) -> Result<(), RendererError> {
        let acc = self.canvas_accumulator();
        self.record_and_submit(|this| {
            this.cmd_clear_image(acc.image, [0.0, 0.0, 0.0, 0.0]);
            Ok(())
        })?;
        for &idx in visible_indices {
            if self.layer_stack.slots[idx].adjustment.is_some() {
                self.apply_adjustment_to(acc, idx)?;
            } else {
                self.compose_layer_into(acc, idx)?;
            }
        }
        Ok(())
    }

    /// Folder-scoped rebuild: like `composite_layers_to_canvas_adjusted` but
    /// walks a bracketed step stream. Each `EnterGroup` pushes a fresh
    /// transparent sub-accumulator; adjustments apply to the top-of-stack
    /// accumulator, so they clip to their enclosing folder; `ExitGroup`
    /// OVER-composites the finished folder onto its parent.
    pub fn composite_layers_scoped(
        &mut self,
        steps: &[CompositeStep],
    ) -> Result<(), RendererError> {
        let root = self.canvas_accumulator();
        self.composite_steps_into(steps, root)
    }

    /// Live mask-edit preview: rebuild the whole canvas into the preview image
    /// with the adjustment at `target_idx` gated by its committed mask merged
    /// with the in-flight stroke, then present. Used while painting an
    /// adjustment layer's mask with the grayscale mask view OFF, so the canvas
    /// shows the effect updating live instead of the black/white mask.
    pub fn render_mask_edit_preview_and_present(
        &mut self,
        steps: &[CompositeStep],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<(), RendererError> {
        self.build_mask_edit_preview(steps, target_idx, color_linear, opacity)?;
        self.present_to_display(PresentSource::Preview)
    }

    /// As [`Self::render_mask_edit_preview_and_present`] but reads the preview
    /// back to host memory (in-stroke export / readback) instead of presenting.
    pub fn render_mask_edit_preview_and_read(
        &mut self,
        steps: &[CompositeStep],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<Vec<u8>, RendererError> {
        self.build_mask_edit_preview(steps, target_idx, color_linear, opacity)?;
        let extent = self.canvas.extent;
        self.read_image_to_staging(self.preview.handle, extent)?;
        self.copy_staging_bytes()
    }

    fn build_mask_edit_preview(
        &mut self,
        steps: &[CompositeStep],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<(), RendererError> {
        let push = [
            color_linear[0],
            color_linear[1],
            color_linear[2],
            opacity.clamp(0.0, 1.0),
        ];
        self.mask_edit = Some(MaskEditPreview {
            target_idx,
            push,
            erase: self.stroke_erase,
        });
        let root = self.preview_accumulator();
        let result = self.composite_steps_into(steps, root);
        self.mask_edit = None;
        result
    }

    /// Walk a bracketed composite step stream into `root`. Each `EnterGroup`
    /// pushes a fresh transparent sub-accumulator; adjustments apply to the
    /// top-of-stack accumulator, so they clip to their enclosing folder;
    /// `ExitGroup` OVER-composites the finished folder onto its parent. A flat
    /// step list (no groups) composites straight into `root`.
    fn composite_steps_into(
        &mut self,
        steps: &[CompositeStep],
        root: Accumulator,
    ) -> Result<(), RendererError> {
        self.record_and_submit(|this| {
            this.cmd_clear_image(root.image, [0.0, 0.0, 0.0, 0.0]);
            Ok(())
        })?;
        // Stack of (accumulator, pool depth). depth 0 = canvas root.
        let mut stack: Vec<Accumulator> = vec![root];
        let mut depth = 0usize;
        for step in steps {
            match *step {
                CompositeStep::Layer(idx) => {
                    let acc = *stack.last().expect("non-empty accumulator stack");
                    if self.layer_stack.slots[idx].adjustment.is_some() {
                        self.apply_adjustment_to(acc, idx)?;
                    } else {
                        self.compose_layer_into(acc, idx)?;
                    }
                }
                CompositeStep::EnterGroup => {
                    let acc = self.ensure_group_accumulator(depth)?;
                    self.record_and_submit(|this| {
                        this.cmd_clear_image(acc.image, [0.0, 0.0, 0.0, 0.0]);
                        Ok(())
                    })?;
                    stack.push(acc);
                    depth += 1;
                }
                CompositeStep::ExitGroup => {
                    let group_acc = stack.pop().expect("ExitGroup without EnterGroup");
                    depth -= 1;
                    let parent = *stack.last().expect("folder nested above the canvas root");
                    let src_set = self.group_accumulators[depth].descriptor_set;
                    self.record_and_submit(|this| {
                        // Make the folder's writes visible to the compose sampler.
                        this.barrier(
                            group_acc.image,
                            vk::ImageLayout::GENERAL,
                            vk::ImageLayout::GENERAL,
                        );
                        this.cmd_compose_layer_blended(
                            parent.image,
                            parent.framebuffer,
                            src_set,
                            0,
                            1.0,
                        );
                        Ok(())
                    })?;
                }
            }
        }
        Ok(())
    }

    /// Folder-scoped live preview: same accumulator-stack walk as
    /// `composite_layers_scoped`, but built into the preview image with the
    /// in-flight stroke merged into the target layer at its place in the tree,
    /// then presented. Used while painting a layer below a folder-bounded
    /// adjustment so the live preview clips like the committed result will.
    /// Full rebuild per frame (no below-cache / dirty-rect), which is acceptable
    /// for this narrow case.
    pub fn render_preview_scoped_and_present(
        &mut self,
        steps: &[CompositeStep],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<(), RendererError> {
        let erase = self.stroke_erase;
        let push = [color_linear[0], color_linear[1], color_linear[2], opacity.clamp(0.0, 1.0)];
        self.build_preview_scoped(steps, target_idx, PreviewTarget::Stroke { push, erase })?;
        self.present_to_display(PresentSource::Preview)
    }

    /// As [`Self::render_preview_scoped_and_present`] but reads the preview back
    /// to host memory (in-stroke export / tests) instead of presenting it.
    pub fn render_preview_scoped_and_read(
        &mut self,
        steps: &[CompositeStep],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<Vec<u8>, RendererError> {
        let erase = self.stroke_erase;
        let push = [color_linear[0], color_linear[1], color_linear[2], opacity.clamp(0.0, 1.0)];
        self.build_preview_scoped(steps, target_idx, PreviewTarget::Stroke { push, erase })?;
        let extent = self.canvas.extent;
        self.read_image_to_staging(self.preview.handle, extent)?;
        self.copy_staging_bytes()
    }

    pub(super) fn build_preview_scoped(
        &mut self,
        steps: &[CompositeStep],
        target_idx: usize,
        target: PreviewTarget,
    ) -> Result<(), RendererError> {
        self.clip = None;
        let spans = scoped_group_spans(steps, target_idx);
        let root = self.preview_accumulator();
        self.record_and_submit(|this| {
            this.cmd_clear_image(root.image, [0.0, 0.0, 0.0, 0.0]);
            Ok(())
        })?;
        let mut frames: Vec<ScopeFrame> = vec![ScopeFrame {
            acc: root,
            fold_set: vk::DescriptorSet::null(),
            cache_ordinal: None,
            used_depth: false,
        }];
        // `depth` counts only the rebuilt folders currently open (the ones that
        // consume a working `group_accumulators` slot); cached folders are
        // skipped whole and never take a slot.
        let mut depth = 0usize;
        let mut built_cache = false;
        let mut i = 0usize;
        while i < steps.len() {
            match steps[i] {
                CompositeStep::Layer(idx) => {
                    let acc = frames.last().expect("non-empty accumulator stack").acc;
                    if idx == target_idx {
                        self.record_and_submit(|this| {
                            this.cmd_compose_preview_target(acc, target_idx, target);
                            Ok(())
                        })?;
                    } else if self.layer_stack.slots[idx].adjustment.is_some() {
                        self.apply_adjustment_to(acc, idx)?;
                    } else {
                        self.compose_layer_into(acc, idx)?;
                    }
                    i += 1;
                }
                CompositeStep::EnterGroup => {
                    let span = spans[i].expect("EnterGroup has a matching span");
                    // A folder without the stroke target is constant this stroke.
                    // If it was cached on an earlier frame, blend the cache in and
                    // jump past its (possibly effect-heavy) interior entirely.
                    let cached = !span.contains_target
                        && self.scoped_cache_valid
                        && self.scoped_group_cache.len() > span.ordinal;
                    if cached {
                        let acc = self.scoped_group_cache_accumulator(span.ordinal);
                        let fold_set = self.scoped_group_cache[span.ordinal].descriptor_set;
                        frames.push(ScopeFrame {
                            acc,
                            fold_set,
                            cache_ordinal: None,
                            used_depth: false,
                        });
                        i = span.exit; // resume at the matching ExitGroup
                    } else {
                        let acc = self.ensure_group_accumulator(depth)?;
                        self.record_and_submit(|this| {
                            this.cmd_clear_image(acc.image, [0.0, 0.0, 0.0, 0.0]);
                            Ok(())
                        })?;
                        let fold_set = self.group_accumulators[depth].descriptor_set;
                        // Persist this folder's result for reuse only if it is
                        // static (does not contain the target).
                        let cache_ordinal = (!span.contains_target).then_some(span.ordinal);
                        frames.push(ScopeFrame {
                            acc,
                            fold_set,
                            cache_ordinal,
                            used_depth: true,
                        });
                        depth += 1;
                        i += 1;
                    }
                }
                CompositeStep::ExitGroup => {
                    let frame = frames.pop().expect("ExitGroup without EnterGroup");
                    if frame.used_depth {
                        depth -= 1;
                    }
                    // Save a freshly built static folder so later frames skip it.
                    if let Some(ord) = frame.cache_ordinal {
                        let cache = self.ensure_scoped_group_cache(ord)?;
                        let src = frame.acc.image;
                        self.record_and_submit(|this| {
                            this.cmd_copy_image_full(src, cache.image);
                            Ok(())
                        })?;
                        built_cache = true;
                    }
                    let parent = frames
                        .last()
                        .expect("folder nested above the canvas root")
                        .acc;
                    let group_img = frame.acc.image;
                    let src_set = frame.fold_set;
                    self.record_and_submit(|this| {
                        this.barrier(
                            group_img,
                            vk::ImageLayout::GENERAL,
                            vk::ImageLayout::GENERAL,
                        );
                        this.cmd_compose_layer_blended(
                            parent.image,
                            parent.framebuffer,
                            src_set,
                            0,
                            1.0,
                        );
                        Ok(())
                    })?;
                    i += 1;
                }
            }
        }
        if built_cache {
            self.scoped_cache_valid = true;
        }
        Ok(())
    }

    /// Lazily allocate the per-stroke static-folder cache up to `ordinal` and
    /// return its accumulator. Mirrors [`Self::ensure_group_accumulator`].
    fn ensure_scoped_group_cache(&mut self, ordinal: usize) -> Result<Accumulator, RendererError> {
        while self.scoped_group_cache.len() <= ordinal {
            let acc = self.create_group_accumulator()?;
            self.scoped_group_cache.push(acc);
        }
        Ok(self.scoped_group_cache_accumulator(ordinal))
    }

    fn scoped_group_cache_accumulator(&self, ordinal: usize) -> Accumulator {
        let ga = &self.scoped_group_cache[ordinal];
        Accumulator {
            image: ga.image.handle,
            view: ga.image.view,
            framebuffer: ga.framebuffer,
        }
    }

    /// Record (no submit) the in-flight target content into `acc` per the
    /// preview's [`PreviewTarget`]: a stroked target copy or the warped layer.
    fn cmd_compose_preview_target(
        &self,
        acc: Accumulator,
        target_idx: usize,
        target: PreviewTarget,
    ) {
        match target {
            PreviewTarget::Stroke { push, erase } => {
                self.preview_compose_stroked_target_into(
                    acc.image,
                    acc.framebuffer,
                    target_idx,
                    push,
                    erase,
                );
            }
            PreviewTarget::Warp { set, mode, opacity, visible } => {
                if visible {
                    self.cmd_compose_layer_blended(acc.image, acc.framebuffer, set, mode, opacity);
                }
            }
            PreviewTarget::Gradient { endpoints, extra } => {
                self.compose_gradient_target_into(
                    acc.image,
                    acc.framebuffer,
                    target_idx,
                    endpoints,
                    extra,
                );
            }
            PreviewTarget::Filter { src_img, set, mode, opacity } => {
                self.barrier(src_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
                self.cmd_compose_layer_blended(acc.image, acc.framebuffer, set, mode, opacity);
            }
        }
    }

    /// Return (allocating on first use) the sub-accumulator for folder nesting
    /// `depth`. Canvas-sized; reused across frames.
    fn ensure_group_accumulator(&mut self, depth: usize) -> Result<Accumulator, RendererError> {
        while self.group_accumulators.len() <= depth {
            let acc = self.create_group_accumulator()?;
            self.group_accumulators.push(acc);
        }
        let ga = &self.group_accumulators[depth];
        Ok(Accumulator {
            image: ga.image.handle,
            view: ga.image.view,
            framebuffer: ga.framebuffer,
        })
    }

    fn create_group_accumulator(&mut self) -> Result<GroupAccumulator, RendererError> {
        let extent = vk::Extent2D {
            width: self.canvas_size.width,
            height: self.canvas_size.height,
        };
        let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED;
        let image = Image::new_2d(
            &self.device,
            &mut self.allocator,
            "group-accumulator",
            CANVAS_FORMAT,
            extent,
            usage,
            vk::ImageAspectFlags::COLOR,
        )?;
        let framebuffer = create_framebuffer_for_view(
            &self.device,
            self.canvas_target.render_pass,
            extent,
            image.view,
        )?;
        let (descriptor_pool, descriptor_set) = create_sampled_image_set(
            &self.device,
            self.layer_composite_pipeline.descriptor_set_layout,
            self.layer_composite_pipeline.sampler,
            image.view,
        )?;
        // Start in GENERAL so the first clear / compose sees a defined layout.
        self.record_and_submit(|this| {
            this.barrier(
                image.handle,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::GENERAL,
            );
            Ok(())
        })?;
        Ok(GroupAccumulator {
            image: std::mem::ManuallyDrop::new(image),
            framebuffer,
            descriptor_pool,
            descriptor_set,
        })
    }

    /// Live in-stroke version of the adjusted composite: build into the preview
    /// image, inserting the in-flight stroke at the target layer's z-position so
    /// the effect chain runs over the stroked result, then present the preview.
    /// Used while painting a layer *below* an adjustment so the effect previews
    /// live instead of snapping in on commit.
    pub fn render_preview_adjusted_and_present(
        &mut self,
        visibilities: &[bool],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<(), RendererError> {
        self.build_preview_adjusted(visibilities, target_idx, color_linear, opacity, true, true)
    }

    /// Same build as the live present path, but read the preview back to host
    /// memory (export / tests) instead of presenting it. Full-canvas (not
    /// incremental) so the whole image is valid.
    pub fn render_preview_adjusted_and_read(
        &mut self,
        visibilities: &[bool],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<Vec<u8>, RendererError> {
        self.build_preview_adjusted(visibilities, target_idx, color_linear, opacity, false, false)?;
        let extent = self.canvas.extent;
        self.read_image_to_staging(self.preview.handle, extent)?;
        self.copy_staging_bytes()
    }

    /// Test/diagnostic: drive the *incremental* adjusted preview (dab-region
    /// clip for local effects) and read it back, to assert it matches a full
    /// rebuild.
    pub fn render_preview_adjusted_incremental_and_read(
        &mut self,
        visibilities: &[bool],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
    ) -> Result<Vec<u8>, RendererError> {
        self.build_preview_adjusted(visibilities, target_idx, color_linear, opacity, false, true)?;
        let extent = self.canvas.extent;
        self.read_image_to_staging(self.preview.handle, extent)?;
        self.copy_staging_bytes()
    }

    fn build_preview_adjusted(
        &mut self,
        visibilities: &[bool],
        target_idx: usize,
        color_linear: [f32; 3],
        opacity: f32,
        present: bool,
        incremental: bool,
    ) -> Result<(), RendererError> {
        let visible_indices = self.visible_layer_indices(visibilities);
        // Always build the below-target stack so the per-frame seed is a clipped
        // copy (the incremental path can't full-clear the persistent preview).
        self.ensure_below_cache(&visible_indices, target_idx)?;
        let push: [f32; 4] = [
            color_linear[0],
            color_linear[1],
            color_linear[2],
            opacity.clamp(0.0, 1.0),
        ];
        let erase = self.stroke_erase;
        // Incremental dirty-rect: recompute only the dab region. Non-local
        // effects (Blur/Stroke) spread, so the effect output region (inner) is
        // the dab expanded by the effect margin, and the seed/input region
        // (outer) is expanded once more so the effect samples correct input.
        // For local effects margin = 0, so inner == outer == dab.
        let (inner, outer) = if incremental {
            match self.take_preview_clip() {
                Some(dab) => {
                    let m = self.adjusted_effect_margin(&visible_indices, target_idx);
                    (Some(self.expand_clip(dab, m)), Some(self.expand_clip(dab, m * 2)))
                }
                None => (None, None),
            }
        } else {
            (None, None)
        };
        // Fast path: record the whole frame in ONE submit when the above-target
        // effect chain fits the input-set ring. The present copy is folded in.
        let result = match self.batched_input_pass_count(&visible_indices, target_idx) {
            Some(n) if n <= INPUT_RING => {
                self.build_preview_adjusted_batched(
                    &visible_indices,
                    target_idx,
                    push,
                    erase,
                    present,
                    inner,
                    outer,
                );
                Ok(())
            }
            _ => {
                // Ring overflow: full-canvas per-submit fallback.
                self.clip = None;
                let r =
                    self.build_preview_adjusted_unbatched(&visible_indices, target_idx, push, erase);
                if present && r.is_ok() {
                    self.present_to_display(PresentSource::Preview)?;
                }
                r
            }
        };
        self.clip = None;
        result
    }

    /// Number of input-set passes the above-target effect chain needs, or `None`
    /// if it contains a stroke effect (recorded by the per-submit fallback).
    fn batched_input_pass_count(&self, visible_indices: &[usize], target_idx: usize) -> Option<usize> {
        let mut n = 0usize;
        for &idx in visible_indices {
            if idx <= target_idx {
                continue;
            }
            let Some(data) = self.layer_stack.slots[idx].adjustment.as_ref() else {
                continue;
            };
            if data.is_noop() {
                continue;
            }
            for effect in &data.effects {
                if !effect.enabled {
                    continue;
                }
                match effect.kind {
                    // filter + mask-mix
                    EffectKind::HueSatBright { .. } | EffectKind::Invert => n += 2,
                    EffectKind::Blur { .. } => n += 3, // H + V + mask-mix
                    EffectKind::Sharpen { .. } => n += 4, // blur H + V + sharpen + mask-mix
                    // The jump-flood band must flood full-canvas (no dab clip),
                    // so a stroke drops the chain to the per-submit path.
                    EffectKind::Stroke { .. } => return None,
                }
            }
        }
        Some(n)
    }

    /// Per-submit fallback: each layer / effect pass is its own fence-waited
    /// submission. Used when the effect chain exceeds the ring or uses a stroke.
    fn build_preview_adjusted_unbatched(
        &mut self,
        visible_indices: &[usize],
        target_idx: usize,
        push: [f32; 4],
        erase: bool,
    ) -> Result<(), RendererError> {
        let acc = self.preview_accumulator();
        if self.preview_cache_valid {
            self.record_and_submit(|this| {
                this.cmd_copy_image_full(this.preview_below.handle, acc.image);
                Ok(())
            })?;
        } else {
            self.record_and_submit(|this| {
                this.cmd_clear_image(acc.image, [0.0, 0.0, 0.0, 0.0]);
                Ok(())
            })?;
        }
        for &idx in visible_indices {
            if idx == target_idx {
                self.record_and_submit(|this| {
                    this.preview_compose_stroked_target(target_idx, push, erase);
                    Ok(())
                })?;
            } else if idx < target_idx {
                if !self.preview_cache_valid {
                    self.compose_layer_into(acc, idx)?;
                }
            } else if self.layer_stack.slots[idx].adjustment.is_some() {
                self.apply_adjustment_to(acc, idx)?;
            } else {
                self.compose_layer_into(acc, idx)?;
            }
        }
        Ok(())
    }

    /// Record the entire adjusted preview frame into one command buffer (one
    /// fence wait): below-stack (cached), stroked target, then the above-target
    /// effect chain using a fresh ring input-set per pass.
    #[allow(clippy::too_many_arguments)]
    fn build_preview_adjusted_batched(
        &mut self,
        visible_indices: &[usize],
        target_idx: usize,
        push: [f32; 4],
        erase: bool,
        present: bool,
        inner: Option<vk::Rect2D>,
        outer: Option<vk::Rect2D>,
    ) {
        let acc = self.preview_accumulator();
        let below_cached = self.preview_cache_valid;
        let visible_indices = visible_indices.to_vec();
        // Blocking: the per-frame effect chain rewrites the shared input-set
        // ring, so this frame's passes must finish before the next frame reuses
        // those sets. The work is one cheap (dirty-rect) submit.
        let _ = self.record_and_submit(|this| {
            // The seed + stroked target use the wider `outer` region so the
            // effect samples correct (raw) input where it reaches past `inner`.
            this.clip = outer;
            // 1. Seed the accumulator with the below-target stack.
            if below_cached {
                this.cmd_copy_image_full(this.preview_below.handle, acc.image);
            } else {
                this.cmd_clear_image(acc.image, [0.0, 0.0, 0.0, 0.0]);
                for &idx in &visible_indices {
                    if idx >= target_idx {
                        break;
                    }
                    this.cmd_compose_layer(acc, idx);
                }
            }
            // 2. The target layer with the in-flight stroke merged in.
            this.preview_compose_stroked_target(target_idx, push, erase);
            // The effect output + above layers + present only touch `inner` (the
            // region that actually changed); effects still sample the accumulator
            // freely, reading the correctly-seeded `outer` region.
            this.clip = inner;
            // 3. Above the target: adjustments (effect chain) + plain layers.
            let mut cursor = 0usize;
            for &idx in &visible_indices {
                if idx <= target_idx {
                    continue;
                }
                if this.layer_stack.slots[idx].adjustment.is_some() {
                    this.cmd_apply_adjustment(acc, idx, &mut cursor);
                } else {
                    this.cmd_compose_layer(acc, idx);
                }
            }
            // 4. Fold the dmabuf present copy into this same submit.
            if present {
                this.record_present_copy(acc.image);
            }
            Ok(())
        });
    }

    /// Record (no submit) a plain layer composite into `acc`.
    fn cmd_compose_layer(&mut self, acc: Accumulator, idx: usize) {
        let set = self.layer_stack.slots[idx].descriptor_set;
        let (mode, opacity) = self.layer_stack.blend(idx);
        self.cmd_compose_layer_blended(acc.image, acc.framebuffer, set, mode, opacity);
    }

    /// Record (no submit) one adjustment's effect chain into the current buffer,
    /// advancing `cursor` through the input-set ring. Only Hsv / Blur effects
    /// reach here (stroke routes to the unbatched path).
    fn cmd_apply_adjustment(&mut self, acc: Accumulator, idx: usize, cursor: &mut usize) {
        let Some(data) = self.layer_stack.slots[idx].adjustment.clone() else {
            return;
        };
        if data.is_noop() {
            return;
        }
        let mask_view = self.layer_stack.slots[idx].image.view;
        let mask_img = self.layer_stack.slots[idx].image.handle;
        let layout = self.filter_resources.pipeline_layout;
        let render_pass = self.canvas_target.render_pass;

        for effect in &data.effects {
            if !effect.enabled {
                continue;
            }
            let Some(spec) = effect_to_filter_spec(effect.kind) else {
                if let EffectKind::Stroke { .. } = effect.kind {
                    self.cmd_apply_stroke(acc, effect.kind, mask_view, mask_img, cursor);
                }
                continue;
            };
            let pre = self.cmd_produce_filtered_passes(acc.view, acc.image, spec, cursor);
            let dst = pre.other();
            let pre_view = self.filter_resources.scratch_view(pre);
            let pre_img = self.filter_resources.scratch_handle(pre);
            let dst_fb = self.filter_resources.framebuffer(dst);
            let mask_mix = self.filter_resources.mask_mix;

            let set = self.filter_resources.input_set(*cursor);
            *cursor += 1;
            self.filter_resources
                .write_input(&self.device, set, pre_view, acc.view, mask_view);
            self.cmd_filter_pass3(
                set, layout, mask_mix, render_pass, dst_fb, pre_img, acc.image, mask_img,
                [1.0, 0.0, 0.0, 0.0],
            );

            let dst_img = self.filter_resources.scratch_handle(dst);
            self.cmd_copy_image_full(dst_img, acc.image);
        }
    }

    /// Record (no submit) a Stroke effect: render the band into scratch A then
    /// OVER-composite it onto `acc`, advancing `cursor`.
    fn cmd_apply_stroke(
        &self,
        acc: Accumulator,
        kind: EffectKind,
        mask_view: ash::vk::ImageView,
        mask_img: ash::vk::Image,
        cursor: &mut usize,
    ) {
        use crate::renderer::filters::Scratch;
        let EffectKind::Stroke {
            color,
            opacity,
            thickness,
            offset,
            softness,
        } = kind
        else {
            return;
        };
        #[allow(clippy::cast_precision_loss)]
        let inv_w = 1.0 / self.canvas.extent.width as f32;
        #[allow(clippy::cast_precision_loss)]
        let inv_h = 1.0 / self.canvas.extent.height as f32;
        let softness_flag =
            f32::from(u8::from(softness == crate::effects::StrokeSoftness::Bilinear));
        let push: [f32; 12] = [
            f32::from(color.r) / 255.0,
            f32::from(color.g) / 255.0,
            f32::from(color.b) / 255.0,
            0.0,
            opacity,
            thickness,
            offset,
            softness_flag,
            inv_w,
            inv_h,
            0.0,
            0.0,
        ];

        self.cmd_stroke_band(acc.view, acc.image, mask_view, mask_img, push, thickness, cursor);

        let comp_set = self.filter_resources.composite_set(Scratch::A);
        let stroke_img = self.filter_resources.scratch_handle(Scratch::A);
        self.barrier(stroke_img, ash::vk::ImageLayout::GENERAL, ash::vk::ImageLayout::GENERAL);
        self.cmd_compose_layer_blended(acc.image, acc.framebuffer, comp_set, 0, 1.0);
    }

    /// Record (no submit) the Hsv / Blur pass chain reading `src` into a scratch
    /// slot, advancing `cursor`. Returns the scratch holding the filtered result.
    fn cmd_produce_filtered_passes(
        &mut self,
        src_view: ash::vk::ImageView,
        src_img: ash::vk::Image,
        spec: FilterSpec,
        cursor: &mut usize,
    ) -> crate::renderer::filters::Scratch {
        use crate::renderer::filters::Scratch;
        let layout = self.filter_resources.pipeline_layout;
        let render_pass = self.canvas_target.render_pass;
        #[allow(clippy::cast_precision_loss)]
        let inv_w = 1.0 / self.canvas.extent.width as f32;
        #[allow(clippy::cast_precision_loss)]
        let inv_h = 1.0 / self.canvas.extent.height as f32;

        match spec {
            FilterSpec::Hsv {
                hue_degrees,
                saturation,
                value,
            } => {
                let push = [hue_degrees.to_radians(), saturation, value, 0.0];
                let pipeline = self.filter_resources.hsv;
                let fb = self.filter_resources.framebuffer(Scratch::A);
                let set = self.filter_resources.input_set(*cursor);
                *cursor += 1;
                self.filter_resources
                    .write_input(&self.device, set, src_view, src_view, src_view);
                self.cmd_filter_pass3(
                    set, layout, pipeline, render_pass, fb, src_img, src_img, src_img, push,
                );
                Scratch::A
            }
            FilterSpec::BoxBlur { radius_x, radius_y } => {
                let pipeline = self.filter_resources.box_blur;
                let fb_a = self.filter_resources.framebuffer(Scratch::A);
                let set_h = self.filter_resources.input_set(*cursor);
                *cursor += 1;
                self.filter_resources
                    .write_input(&self.device, set_h, src_view, src_view, src_view);
                self.cmd_filter_pass3(
                    set_h, layout, pipeline, render_pass, fb_a, src_img, src_img, src_img,
                    [inv_w, 0.0, radius_x, 0.0],
                );

                let a_view = self.filter_resources.scratch_view(Scratch::A);
                let a_img = self.filter_resources.scratch_handle(Scratch::A);
                let fb_b = self.filter_resources.framebuffer(Scratch::B);
                let set_v = self.filter_resources.input_set(*cursor);
                *cursor += 1;
                self.filter_resources
                    .write_input(&self.device, set_v, a_view, a_view, a_view);
                self.cmd_filter_pass3(
                    set_v, layout, pipeline, render_pass, fb_b, a_img, a_img, a_img,
                    [0.0, inv_h, radius_y, 0.0],
                );
                Scratch::B
            }
            FilterSpec::Invert => {
                let pipeline = self.filter_resources.invert;
                let fb = self.filter_resources.framebuffer(Scratch::A);
                let set = self.filter_resources.input_set(*cursor);
                *cursor += 1;
                self.filter_resources
                    .write_input(&self.device, set, src_view, src_view, src_view);
                self.cmd_filter_pass3(
                    set, layout, pipeline, render_pass, fb, src_img, src_img, src_img, [0.0; 4],
                );
                Scratch::A
            }
            FilterSpec::Sharpen { amount } => {
                // Blur the source (H then V) into B, then unsharp = src vs blurred.
                let r = FilterSpec::SHARPEN_BLUR_RADIUS;
                let blur = self.filter_resources.box_blur;
                let fb_a = self.filter_resources.framebuffer(Scratch::A);
                let set_h = self.filter_resources.input_set(*cursor);
                *cursor += 1;
                self.filter_resources
                    .write_input(&self.device, set_h, src_view, src_view, src_view);
                self.cmd_filter_pass3(
                    set_h, layout, blur, render_pass, fb_a, src_img, src_img, src_img,
                    [inv_w, 0.0, r, 0.0],
                );

                let a_view = self.filter_resources.scratch_view(Scratch::A);
                let a_img = self.filter_resources.scratch_handle(Scratch::A);
                let fb_b = self.filter_resources.framebuffer(Scratch::B);
                let set_v = self.filter_resources.input_set(*cursor);
                *cursor += 1;
                self.filter_resources
                    .write_input(&self.device, set_v, a_view, a_view, a_view);
                self.cmd_filter_pass3(
                    set_v, layout, blur, render_pass, fb_b, a_img, a_img, a_img,
                    [0.0, inv_h, r, 0.0],
                );

                // Sharpen reads the source (binding 0) + blurred-in-B (binding 1).
                let b_view = self.filter_resources.scratch_view(Scratch::B);
                let b_img = self.filter_resources.scratch_handle(Scratch::B);
                let sharpen = self.filter_resources.sharpen;
                let set_s = self.filter_resources.input_set(*cursor);
                *cursor += 1;
                self.filter_resources
                    .write_input(&self.device, set_s, src_view, b_view, b_view);
                self.cmd_filter_pass3(
                    set_s, layout, sharpen, render_pass, fb_a, src_img, b_img, b_img,
                    [amount, 0.0, 0.0, 0.0],
                );
                Scratch::A
            }
        }
    }

    /// Show an adjustment layer's grayscale mask on the canvas (its slot is the
    /// mask) so the user can see/paint it while it is the active layer.
    pub fn render_mask_preview(&mut self, idx: usize) -> Result<(), RendererError> {
        let set = self.layer_stack.slots[idx].descriptor_set;
        let acc = self.preview_accumulator();
        self.record_and_submit(|this| {
            this.cmd_clear_image(acc.image, [0.0, 0.0, 0.0, 0.0]);
            this.cmd_compose_layer_blended(acc.image, acc.framebuffer, set, 0, 1.0);
            Ok(())
        })
    }

    /// Bake adjustment effects for the layers strictly below `target_idx` into
    /// the `preview_below` cache, so the fast in-stroke path (which copies that
    /// cache and skips adjustment slots) still shows the adjusted backdrop when
    /// painting a layer *above* an adjustment. Multi-submit but runs once per
    /// stroke. No-op (leaving the cache to the fast path) when no effective
    /// adjustment sits below the target.
    pub(super) fn prepare_below_cache_if_needed(
        &mut self,
        visible_indices: &[usize],
        target_idx: usize,
    ) -> Result<(), RendererError> {
        if self.preview_cache_valid {
            return Ok(());
        }
        // Only take over the (multi-submit) adjustment-aware build for the fast
        // default path when an adjustment actually sits below the target;
        // otherwise leave the cache to record_layered_preview's single submit.
        let has_effective_adjustment_below = visible_indices
            .iter()
            .any(|&i| i < target_idx && self.slot_is_effective_adjustment(i));
        if !has_effective_adjustment_below {
            return Ok(());
        }
        self.ensure_below_cache(visible_indices, target_idx)
    }

    /// Build the below-target stack into `preview_below` (adjustment-aware) if
    /// the cache is stale. Always builds, so the adjusted preview can seed the
    /// accumulator with a clipped copy rather than a full clear (needed for the
    /// incremental dirty-rect path).
    fn ensure_below_cache(
        &mut self,
        visible_indices: &[usize],
        target_idx: usize,
    ) -> Result<(), RendererError> {
        if self.preview_cache_valid {
            return Ok(());
        }
        let acc = self.preview_below_accumulator();
        self.record_and_submit(|this| {
            this.cmd_clear_image(acc.image, [0.0, 0.0, 0.0, 0.0]);
            Ok(())
        })?;
        for &idx in visible_indices {
            if idx >= target_idx {
                break;
            }
            if self.layer_stack.slots[idx].adjustment.is_some() {
                self.apply_adjustment_to(acc, idx)?;
            } else {
                self.compose_layer_into(acc, idx)?;
            }
        }
        self.preview_cache_valid = true;
        Ok(())
    }

    /// Spatial radius (canvas pixels) the above-target effect chain reaches:
    /// a change in the input affects output within this distance. Local effects
    /// (Hue/Sat/Bright) contribute 0; Blur its radius; Stroke its thickness.
    /// Summed across the chain (each effect can spread the previous one).
    fn adjusted_effect_margin(&self, visible_indices: &[usize], target_idx: usize) -> u32 {
        let mut margin = 0u32;
        for &idx in visible_indices {
            if idx <= target_idx {
                continue;
            }
            let Some(data) = self.layer_stack.slots[idx].adjustment.as_ref() else {
                continue;
            };
            if data.is_noop() {
                continue;
            }
            for effect in &data.effects {
                if !effect.enabled {
                    continue;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let r = match effect.kind {
                    EffectKind::HueSatBright { .. } | EffectKind::Invert => 0,
                    EffectKind::Blur { radius_x, radius_y } => {
                        radius_x.max(radius_y).ceil().max(0.0) as u32
                    }
                    EffectKind::Sharpen { .. } => FilterSpec::SHARPEN_BLUR_RADIUS.ceil() as u32,
                    EffectKind::Stroke { thickness, .. } => thickness.ceil().max(0.0) as u32 + 1,
                };
                margin += r;
            }
        }
        margin
    }

    /// Expand `r` by `margin` canvas pixels, clamped to the canvas bounds.
    fn expand_clip(&self, r: vk::Rect2D, margin: u32) -> vk::Rect2D {
        let m = margin as i32;
        #[allow(clippy::cast_possible_wrap)]
        let (cw, ch) = (self.canvas.extent.width as i32, self.canvas.extent.height as i32);
        let x0 = (r.offset.x - m).max(0);
        let y0 = (r.offset.y - m).max(0);
        #[allow(clippy::cast_possible_wrap)]
        let x1 = (r.offset.x + r.extent.width as i32 + m).min(cw);
        #[allow(clippy::cast_possible_wrap)]
        let y1 = (r.offset.y + r.extent.height as i32 + m).min(ch);
        #[allow(clippy::cast_sign_loss)]
        vk::Rect2D {
            offset: vk::Offset2D { x: x0, y: y0 },
            extent: vk::Extent2D {
                width: (x1 - x0).max(0) as u32,
                height: (y1 - y0).max(0) as u32,
            },
        }
    }

    fn slot_is_effective_adjustment(&self, idx: usize) -> bool {
        self.layer_stack
            .slots
            .get(idx)
            .and_then(|s| s.adjustment.as_ref())
            .is_some_and(|d| !d.is_noop())
    }

    fn preview_below_accumulator(&self) -> Accumulator {
        Accumulator {
            image: self.preview_below.handle,
            view: self.preview_below.view,
            framebuffer: self.preview_below_framebuffer,
        }
    }

    fn canvas_accumulator(&self) -> Accumulator {
        Accumulator {
            image: self.canvas.handle,
            view: self.canvas.view,
            framebuffer: self.canvas_target.framebuffer,
        }
    }

    fn preview_accumulator(&self) -> Accumulator {
        Accumulator {
            image: self.preview.handle,
            view: self.preview.view,
            framebuffer: self.preview_framebuffer,
        }
    }

    /// Blend one plain layer image into `acc` at its blend mode + opacity, on
    /// its own submission. Adjustment slots are skipped (handled separately).
    fn compose_layer_into(&mut self, acc: Accumulator, idx: usize) -> Result<(), RendererError> {
        let descriptor_set = self.layer_stack.slots[idx].descriptor_set;
        let (mode, opacity) = self.layer_stack.blend(idx);
        self.record_and_submit(|this| {
            this.cmd_compose_layer_blended(acc.image, acc.framebuffer, descriptor_set, mode, opacity);
            Ok(())
        })
    }

    /// Run the adjustment layer at `idx`'s effect stack against `acc`, gated by
    /// the layer's own grayscale mask. Each enabled effect: filter the
    /// accumulator -> mask-mix the result over the untouched accumulator by the
    /// mask -> copy back into the accumulator.
    ///
    /// While a mask-edit preview is building (`self.mask_edit` names this slot),
    /// the gate is the committed mask MERGED with the in-flight stroke, so the
    /// effect previews the mask the user is painting without ever showing the
    /// grayscale mask itself.
    fn apply_adjustment_to(&mut self, acc: Accumulator, idx: usize) -> Result<(), RendererError> {
        let (mask_view, mask_img) = match self.mask_edit {
            Some(me) if me.target_idx == idx => self.stroked_mask(idx, me.push, me.erase)?,
            _ => {
                let slot = &self.layer_stack.slots[idx];
                (slot.image.view, slot.image.handle)
            }
        };
        self.apply_adjustment_with_mask(acc, idx, mask_view, mask_img)
    }

    /// Build the committed mask of slot `idx` merged with the in-flight stroke
    /// into the erase-preview scratch, returning that scratch's (view, image).
    /// Used by the mask-edit preview so the effect gates on the live mask.
    fn stroked_mask(
        &mut self,
        idx: usize,
        push: [f32; 4],
        erase: bool,
    ) -> Result<(vk::ImageView, vk::Image), RendererError> {
        let mask_image = self.layer_stack.slots[idx].image.handle;
        let scratch = self.erase_preview.scratch.handle;
        let scratch_fb = self.erase_preview.framebuffer;
        self.record_and_submit(|this| {
            this.cmd_copy_image_full(mask_image, scratch);
            this.cmd_composite_stroke(scratch_fb, push, erase);
            this.barrier(scratch, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            Ok(())
        })?;
        Ok((self.erase_preview.scratch.view, scratch))
    }

    /// As [`Self::apply_adjustment_to`] but with the gating mask supplied
    /// explicitly (the committed slot image, or a stroke-merged scratch).
    fn apply_adjustment_with_mask(
        &mut self,
        acc: Accumulator,
        idx: usize,
        mask_view: vk::ImageView,
        mask_img: vk::Image,
    ) -> Result<(), RendererError> {
        // Clone the (small, params-only) effect stack so we are not borrowing
        // the slot while mutating the renderer through the pass helpers.
        let Some(data): Option<AdjustmentData> = self.layer_stack.slots[idx].adjustment.clone()
        else {
            return Ok(());
        };
        if data.is_noop() {
            return Ok(());
        }

        for effect in &data.effects {
            if !effect.enabled {
                continue;
            }
            let Some(spec) = effect_to_filter_spec(effect.kind) else {
                if let EffectKind::Stroke { .. } = effect.kind {
                    self.apply_stroke_to(acc, effect.kind, mask_view, mask_img)?;
                }
                continue;
            };

            // Filter the accumulator (read-only) into a scratch slot.
            let pre = self.produce_filtered_passes(acc.view, acc.image, spec)?;
            let dst = pre.other();
            let pre_view = self.filter_resources.scratch_view(pre);
            let pre_img = self.filter_resources.scratch_handle(pre);

            // mix(accumulator, filtered, mask.r) -> dst. params.x = 1 makes the
            // shader honour the mask (white = full effect, black = untouched).
            self.filter_pass3(
                self.filter_resources.mask_mix,
                dst,
                pre_view,
                pre_img,
                acc.view,
                acc.image,
                mask_view,
                mask_img,
                [1.0, 0.0, 0.0, 0.0],
            )?;

            // Replace the accumulator with the adjusted result.
            let dst_img = self.filter_resources.scratch_handle(dst);
            self.record_and_submit(|this| {
                this.cmd_copy_image_full(dst_img, acc.image);
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Stroke effect: render the band into scratch A reading `acc` as the
    /// backdrop silhouette, then OVER-composite it onto `acc`.
    fn apply_stroke_to(
        &mut self,
        acc: Accumulator,
        kind: EffectKind,
        mask_view: vk::ImageView,
        mask_img: vk::Image,
    ) -> Result<(), RendererError> {
        let EffectKind::Stroke {
            color,
            opacity,
            thickness,
            offset,
            softness,
        } = kind
        else {
            return Ok(());
        };

        #[allow(clippy::cast_precision_loss)]
        let inv_w = 1.0 / self.canvas.extent.width as f32;
        #[allow(clippy::cast_precision_loss)]
        let inv_h = 1.0 / self.canvas.extent.height as f32;
        let softness_flag = f32::from(u8::from(softness == crate::effects::StrokeSoftness::Bilinear));

        // 3x vec4: color (rgb, _), params (opacity, thickness, offset, softness),
        // texel (1/w, 1/h, _, _).
        let push: [f32; 12] = [
            f32::from(color.r) / 255.0,
            f32::from(color.g) / 255.0,
            f32::from(color.b) / 255.0,
            0.0,
            opacity,
            thickness,
            offset,
            softness_flag,
            inv_w,
            inv_h,
            0.0,
            0.0,
        ];

        // Build the jump-flood band into Scratch::A and OVER-blend it onto the
        // accumulator, all in one submit. The scoped/unbatched preview paths run
        // this with `clip` unset, so the flood is full-canvas (correct distance
        // propagation); a Stroke effect forces those paths via
        // `batched_input_pass_count`.
        use crate::renderer::filters::Scratch;
        let set = self.filter_resources.composite_set(Scratch::A);
        let stroke_img = self.filter_resources.scratch_handle(Scratch::A);
        self.record_and_submit(|this| {
            let mut cursor = 0usize;
            this.cmd_stroke_band(acc.view, acc.image, mask_view, mask_img, push, thickness, &mut cursor);
            this.barrier(stroke_img, vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL);
            this.cmd_compose_layer_blended(acc.image, acc.framebuffer, set, 0, 1.0);
            Ok(())
        })
    }
}
