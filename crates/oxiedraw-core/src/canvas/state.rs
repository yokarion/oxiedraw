use oxiedraw_utils::geometry::{Size, TransformFilter, TransformRect};
use oxiedraw_utils::pixels::{crop_bgra8, transform_bgra8};

use crate::brush_engine::PaintTarget;
use crate::color::Color;
use crate::document::{
    build_composite_steps, BlendMode, CompositeStep, LayerKind, LayerState, LayerTreeNode,
};
use crate::effects::AdjustmentData;
use crate::filters::FilterSpec;
use crate::renderer::{
    DmabufDescriptor, EdgesBuffer, GradientKind, RendererError, SelectionBlendMode, ShapeKind,
    VulkanRenderer,
};
use crate::selection::SelectionShape;
use crate::tools::{CropRect, SelectionMode};

use super::stamp::{BatchStamp, StrokeStamp};

/// Captured stroke context - what was true when the user pressed down.
/// Held so a mid-drag change of color, opacity, or active layer
/// doesn't take effect until the next stroke.
#[derive(Debug, Clone, Copy)]
struct StrokeContext {
    color: Color,
    opacity: f32,
    layer_idx: usize,
}

/// Headless drawing surface.
///
/// Owns the Vulkan renderer + a shared [`LayerState`]. Brush strokes
/// target the active layer; the displayable canvas image is a
/// composite of all visible layers, rebuilt as needed.
///
/// This is the layer the UI talks to. It deliberately knows nothing
/// about GTK - the canvas widget calls [`Canvas::present`] to drive
/// the zero-copy dmabuf display path.
pub struct Canvas {
    renderer: VulkanRenderer,
    layers: LayerState,
    /// `Some` while a stroke is in flight (between `begin_stroke` and
    /// `commit_stroke`/`discard_stroke`).
    current_stroke: Option<StrokeContext>,
    /// Monotonic counter bumped on every mutation that changes what
    /// would be displayed. UI keys its frame cache on this.
    pixels_version: u64,
    /// Last `pixels_version` that was actually presented to the dmabuf
    /// display image. `present()` short-circuits when this matches.
    display_version: u64,
    /// The adjustment layer (if any) whose mask is currently shown on the
    /// canvas because it is the active layer. Tracked so `present()` re-renders
    /// when the selection switches into or out of mask view, independent of
    /// `pixels_version`.
    displayed_mask_idx: Option<usize>,
    /// Id of the adjustment layer whose mask the user has toggled into view (via
    /// the layer-row mask button). The mask is shown on the canvas only while
    /// this matches an adjustment layer; otherwise the normal composite shows.
    mask_view_id: Option<String>,
    /// Folder structure over the flat layer stack, pushed from the UI. Empty =
    /// flat. Used to scope adjustment layers to their enclosing folder at
    /// composite time and persisted with the document.
    layer_tree: Vec<LayerTreeNode>,
}

impl Canvas {
    /// Construct a Canvas backed by the provided [`LayerState`]. The
    /// renderer's per-layer image stack is initialised to mirror the
    /// state - one GPU image per existing entry, all cleared to
    /// transparent. The active layer is set to the first one if no
    /// active selection is present.
    pub fn new(size: Size, layers: LayerState) -> Result<Self, RendererError> {
        let mut renderer = VulkanRenderer::new(size)?;
        renderer.clear_stroke()?;
        let initial_count = layers.len();
        // Allocate one GPU image per existing layer.
        for _ in 0..initial_count {
            renderer.add_layer()?;
        }
        if layers.active().is_none() && initial_count > 0 {
            layers.set_active(Some(0));
        }
        let mut canvas = Self {
            renderer,
            layers,
            current_stroke: None,
            pixels_version: 1,
            display_version: 0,
            displayed_mask_idx: None,
            mask_view_id: None,
            layer_tree: Vec::new(),
        };
        // Initial canvas state == empty layer stack composited. Yields
        // a fully-transparent canvas regardless of layer count.
        canvas.recomposite_canvas()?;
        Ok(canvas)
    }

    /// Convenience for tests: a Canvas with a single default
    /// "Background" layer pre-created.
    pub fn headless(size: Size) -> Result<Self, RendererError> {
        let layers = LayerState::new();
        layers.add("Background");
        layers.set_active(Some(0));
        Self::new(size, layers)
    }

    #[must_use]
    pub const fn size(&self) -> Size {
        self.renderer.canvas_size()
    }

    /// Read-only handle to the layer state. UI panels render from
    /// this; mutations must go through the `add_layer` / `remove_layer`
    /// / `reorder_layer` / `set_layer_visible` methods so the GPU
    /// stack stays in sync.
    #[must_use]
    pub const fn layers(&self) -> &LayerState {
        &self.layers
    }

    #[must_use]
    pub const fn pixels_version(&self) -> u64 {
        self.pixels_version
    }

    /// Per-layer content version (bumped whenever that layer's pixels change).
    /// The layers panel uses it to re-read only the layers that changed instead
    /// of all of them on every edit.
    #[must_use]
    pub fn layer_content_version(&self, idx: usize) -> u64 {
        self.renderer.layer_content_version(idx)
    }

    const fn bump_version(&mut self) {
        self.pixels_version = self.pixels_version.wrapping_add(1);
    }

    // ----------------------------------------------------------------
    // Stroke lifecycle
    // ----------------------------------------------------------------

    /// Start a new stroke. Captures `(color, opacity)` *and* the
    /// active layer index so the eventual commit targets the layer
    /// the user actually picked when they pressed down. `erase` makes the
    /// stroke remove coverage from the target layer instead of painting.
    pub fn begin_stroke(
        &mut self,
        color: Color,
        opacity: f32,
        erase: bool,
    ) -> Result<(), RendererError> {
        let Some(layer_idx) = self.layers.active() else {
            // No active layer = nothing to draw on. Silently skip;
            // brush events will be no-ops because `is_drawing()` on
            // the engine will stay false.
            tracing::warn!("begin_stroke with no active layer - stroke ignored");
            return Ok(());
        };
        if layer_idx >= self.renderer.layer_count() {
            return Err(RendererError::LayerIndexOutOfRange);
        }

        // An adjustment layer's slot is a grayscale mask: keep paint neutral and
        // never erase (erasing would punch transparency the mask can't hold).
        let is_adjustment = self
            .layers
            .kind(layer_idx)
            .is_some_and(|k| k.is_adjustment());
        let (color, erase) = if is_adjustment {
            (color.to_grayscale(), false)
        } else {
            (color, erase)
        };

        self.renderer.set_stroke_erase(erase);
        // Default to MAX-blend; a build-up brush opts in via
        // `set_stroke_buildup(true)` right after this call.
        self.renderer.set_stroke_buildup(false);
        self.renderer.clear_stroke()?;
        // New stroke target / fresh layer state: the cached below-stack
        // composite must be rebuilt on the first preview of this stroke.
        self.renderer.invalidate_preview_cache();
        // Start a fresh dirty-rect so the history patch covers only this
        // stroke's dabs.
        self.renderer.reset_stroke_dirty();
        self.current_stroke = Some(StrokeContext {
            color,
            opacity,
            layer_idx,
        });
        self.bump_version();
        Ok(())
    }

    /// Opt the in-flight stroke into build-up (accumulating OVER-blend in
    /// the stroke buffer). Call right after `begin_stroke`. The stroke then
    /// builds up where it overlaps itself and caps at the stroke opacity on
    /// the single commit composite - no per-event flushing needed.
    pub fn set_stroke_buildup(&mut self, buildup: bool) {
        self.renderer.set_stroke_buildup(buildup);
    }

    /// Run `paint` with a [`PaintTarget`] that stamps dabs into the
    /// stroke buffer. Wraps each `BrushEngine::begin_stroke /
    /// push_sample / end_stroke` call.
    pub fn stamp<F>(&mut self, paint: F) -> Result<(), RendererError>
    where
        F: FnOnce(&mut dyn PaintTarget),
    {
        let mut adapter = StrokeStamp::new(&mut self.renderer);
        paint(&mut adapter);
        let result = adapter.into_result();
        self.bump_version();
        result
    }

    /// Composite the in-flight stroke buffer into the active layer
    /// (the one captured by `begin_stroke`), then clear the stroke
    /// buffer and re-composite the canvas from the layer stack so
    /// `present()` shows the new state. No-op without a stroke.
    pub fn commit_stroke(&mut self) -> Result<(), RendererError> {
        let Some(ctx) = self.current_stroke.take() else {
            return Ok(());
        };
        let linear = ctx.color.to_linear_rgb();
        let visibilities = self.visibilities();
        self.renderer
            .commit_stroke_into_layer(ctx.layer_idx, linear, ctx.opacity, &visibilities)?;
        // commit_stroke_into_layer composites flat; redo it folder-scoped if any
        // folder bounds an adjustment, so the committed result clips correctly.
        self.rescope_composite()?;
        // Clear erase mode now the stroke is done so a later composite that
        // does not start with `begin_stroke` cannot inherit it.
        self.renderer.set_stroke_erase(false);
        self.renderer.invalidate_preview_cache();
        self.bump_version();
        Ok(())
    }

    /// Re-run the composite with folder scoping when a folder bounds an
    /// adjustment. No-op (cheap check) otherwise. Used after composite paths
    /// that build the canvas flat (the stroke commit).
    fn rescope_composite(&mut self) -> Result<(), RendererError> {
        let snapshot = self.layers.snapshot();
        if let Some(steps) = self.folder_scoped_steps(&snapshot) {
            self.renderer.composite_layers_scoped(&steps)?;
        }
        Ok(())
    }

    /// Discard the in-flight stroke without compositing it.
    pub fn discard_stroke(&mut self) -> Result<(), RendererError> {
        self.current_stroke = None;
        self.renderer.clear_stroke()?;
        self.renderer.set_stroke_erase(false);
        self.bump_version();
        Ok(())
    }

    // ----------------------------------------------------------------
    // Layer management
    // ----------------------------------------------------------------

    /// Append a new layer to the document. Both [`LayerState`] and the
    /// renderer's image stack grow in lockstep; the canvas is
    /// re-composited (no visible change since the new layer is empty).
    pub fn add_layer(&mut self, name: impl Into<String>) -> Result<usize, RendererError> {
        let idx = self.renderer.add_layer()?;
        let state_idx = self.layers.add(name);
        debug_assert_eq!(idx, state_idx, "renderer and state must stay in lockstep");
        self.layers.set_active(Some(idx));
        self.recomposite_canvas()?;
        Ok(idx)
    }

    /// Add a non-destructive adjustment layer on top of the stack. Its image
    /// slot is its grayscale mask, initialised to opaque white (the effects
    /// apply everywhere until the user paints the mask). Returns the new
    /// layer's index.
    pub fn add_adjustment_layer(
        &mut self,
        name: impl Into<String>,
    ) -> Result<usize, RendererError> {
        let idx = self.renderer.add_layer()?;
        let state_idx = self.layers.add(name);
        debug_assert_eq!(idx, state_idx, "renderer and state must stay in lockstep");
        let data = AdjustmentData::default();
        self.layers
            .set_kind(idx, LayerKind::Adjustment(data.clone()));
        // White mask = full-strength effect across the whole canvas.
        self.renderer.clear_layer(idx, [1.0, 1.0, 1.0, 1.0])?;
        self.renderer.set_layer_adjustment(idx, Some(data));
        self.layers.set_active(Some(idx));
        self.recomposite_canvas()?;
        Ok(idx)
    }

    /// Replace the effect stack of the adjustment layer at `idx` and
    /// re-composite. The mask (the layer's pixels) is untouched.
    pub fn set_layer_effects(
        &mut self,
        idx: usize,
        data: AdjustmentData,
    ) -> Result<(), RendererError> {
        self.layers
            .set_kind(idx, LayerKind::Adjustment(data.clone()));
        self.renderer.set_layer_adjustment(idx, Some(data));
        self.recomposite_canvas()?;
        Ok(())
    }

    /// The effect stack of the adjustment layer at `idx`, if it is one.
    #[must_use]
    pub fn layer_effects(&self, idx: usize) -> Option<AdjustmentData> {
        match self.layers.kind(idx) {
            Some(LayerKind::Adjustment(data)) => Some(data),
            _ => None,
        }
    }

    /// Remove the layer at `idx`. Active selection moves to the
    /// previous layer (or `None` if the stack is now empty).
    pub fn remove_layer(&mut self, idx: usize) -> Result<(), RendererError> {
        self.renderer.remove_layer(idx)?;
        self.layers.remove(idx);
        self.recomposite_canvas()?;
        Ok(())
    }

    // Folds the listed layers into the lowest-indexed one and drops the rest;
    // returns the surviving layer's index. Needs at least two distinct indices.
    pub fn merge_layers(&mut self, indices: &[usize]) -> Result<usize, RendererError> {
        let mut sorted = indices.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() < 2 {
            return Err(RendererError::LayerIndexOutOfRange);
        }
        if *sorted.last().expect("sorted has >=2 entries per check above") >= self.renderer.layer_count() {
            return Err(RendererError::LayerIndexOutOfRange);
        }

        // Composite the selected layers together honoring each one's blend
        // mode + opacity, so the flattened raster matches what the user saw.
        let mut merged = Vec::new();
        self.renderer.read_layers_composited(&sorted, &mut merged)?;

        let target = sorted[0];
        self.renderer.write_layer(target, &merged)?;
        // The blend is now baked into the pixels; the survivor composites
        // Normal/opaque from here so it is not blended a second time.
        self.layers.set_blend(target, BlendMode::Normal, 1.0);
        self.renderer
            .set_layer_blend(target, BlendMode::Normal.to_gpu(), 1.0);

        for &idx in sorted[1..].iter().rev() {
            self.renderer.remove_layer(idx)?;
            self.layers.remove(idx);
        }

        self.recomposite_canvas()?;
        Ok(target)
    }

    /// Duplicate the layer at `src_idx`. The copy is placed directly above
    /// `src_idx`, named `"<original> copy"`, and becomes the active layer.
    /// Returns the new layer's index.
    pub fn duplicate_layer(&mut self, src_idx: usize) -> Result<usize, RendererError> {
        let pixels = self.renderer.read_layer(src_idx)?;
        let src_name = self
            .layers
            .snapshot()
            .get(src_idx)
            .map_or_else(|| "Layer".to_string(), |l| l.name.clone());
        let new_name = format!("{src_name} copy");

        let (src_blend, src_opacity) = self
            .layers
            .blend(src_idx)
            .unwrap_or((BlendMode::Normal, 1.0));

        let top_idx = self.renderer.add_layer()?;
        let state_idx = self.layers.add(new_name);
        debug_assert_eq!(
            top_idx, state_idx,
            "renderer and state must stay in lockstep"
        );
        self.renderer.write_layer(top_idx, &pixels)?;

        let target_idx = src_idx + 1;
        if target_idx < top_idx {
            self.layers.reorder(top_idx, target_idx);
            self.renderer.reorder_layer(top_idx, target_idx);
        }

        // The duplicate inherits the source's blend mode + opacity.
        self.layers.set_blend(target_idx, src_blend, src_opacity);
        self.renderer
            .set_layer_blend(target_idx, src_blend.to_gpu(), src_opacity);

        self.layers.set_active(Some(target_idx));
        self.recomposite_canvas()?;
        Ok(target_idx)
    }

    /// Add a new layer on top of the stack, fill it with the provided
    /// BGRA8 pixel data (`canvas_w x canvas_h`, row-major), and make it
    /// active. Returns the new layer's index.
    pub fn add_layer_with_pixels(
        &mut self,
        name: impl Into<String>,
        pixels: &[u8],
    ) -> Result<usize, RendererError> {
        let idx = self.renderer.add_layer()?;
        let state_idx = self.layers.add(name);
        debug_assert_eq!(idx, state_idx, "renderer and state must stay in lockstep");
        self.renderer.write_layer(idx, pixels)?;
        self.layers.set_active(Some(idx));
        self.recomposite_canvas()?;
        Ok(idx)
    }

    /// Move a layer from `from` to `to`. Both metadata + renderer
    /// stack reorder identically; canvas is re-composited.
    pub fn reorder_layer(&mut self, from: usize, to: usize) -> Result<(), RendererError> {
        self.layers.reorder(from, to);
        self.renderer.reorder_layer(from, to);
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Toggle a layer's visibility. Re-composites.
    pub fn set_layer_visible(&mut self, idx: usize, visible: bool) -> Result<(), RendererError> {
        self.layers.set_visible(idx, visible);
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Set the blend mode + opacity of one or more layers and re-composite the
    /// canvas once. Both the document state and the GPU layer slots are updated
    /// so the change survives the next composite and a save/load round-trip.
    pub fn set_layers_blend(
        &mut self,
        changes: &[(usize, BlendMode, f32)],
    ) -> Result<(), RendererError> {
        for &(idx, blend, opacity) in changes {
            self.layers.set_blend(idx, blend, opacity);
            self.renderer.set_layer_blend(idx, blend.to_gpu(), opacity);
        }
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Set the blend mode + opacity of a single layer (convenience wrapper).
    pub fn set_layer_blend(
        &mut self,
        idx: usize,
        blend: BlendMode,
        opacity: f32,
    ) -> Result<(), RendererError> {
        self.set_layers_blend(&[(idx, blend, opacity)])
    }

    /// Replace the entire layer stack with `(id, name, visible, blend, opacity,
    /// bgra8_pixels)` entries. Discards any in-flight stroke.
    pub fn replace_all_layers(
        &mut self,
        layers: &[(String, String, bool, BlendMode, f32, Vec<u8>)],
    ) -> Result<(), RendererError> {
        self.current_stroke = None;

        while self.renderer.layer_count() > 0 {
            self.renderer.remove_layer(0)?;
        }
        self.layers.clear();

        for (id, name, visible, blend, opacity, pixels) in layers {
            let gpu_idx = self.renderer.add_layer()?;
            let state_idx = self.layers.add_full(id.clone(), name.as_str(), *visible);
            debug_assert_eq!(
                gpu_idx, state_idx,
                "renderer and state must stay in lockstep"
            );
            self.layers.set_blend(state_idx, *blend, *opacity);
            self.renderer.set_layer_blend(gpu_idx, blend.to_gpu(), *opacity);
            self.renderer.write_layer(gpu_idx, pixels)?;
        }

        if !layers.is_empty() {
            self.layers.set_active(Some(0));
        }
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Replace the folder structure (pushed from the UI layers panel) and
    /// recomposite, since folder bounds scope adjustment layers. Children are in
    /// canvas order (bottom-to-top). Empty = flat.
    pub fn set_layer_tree(&mut self, tree: Vec<LayerTreeNode>) -> Result<(), RendererError> {
        self.layer_tree = tree;
        self.recomposite_canvas()
    }

    /// Replace the folder structure without recompositing. For metadata-only
    /// changes (folder renamed or expanded/collapsed) that do not alter the
    /// composited image but should still be persisted.
    pub fn set_layer_tree_quiet(&mut self, tree: Vec<LayerTreeNode>) {
        self.layer_tree = tree;
    }

    /// The current folder structure (for persistence / the UI to read back).
    #[must_use]
    pub fn layer_tree(&self) -> &[LayerTreeNode] {
        &self.layer_tree
    }

    /// Re-composite the canvas image from the current layer state.
    /// Called automatically after any layer-affecting mutation.
    fn recomposite_canvas(&mut self) -> Result<(), RendererError> {
        let snapshot = self.layers.snapshot();
        let visibilities: Vec<bool> = snapshot.iter().map(|l| l.visible).collect();
        // Folder-scoped path only when there are adjustments to clip AND the
        // tree actually nests them; otherwise the flat composite is identical.
        if let Some(steps) = self.folder_scoped_steps(&snapshot) {
            self.renderer.composite_layers_scoped(&steps)?;
        } else {
            self.renderer.composite_layers_to_canvas(&visibilities)?;
        }
        // Layer images / stack changed: the cached preview below-stack is stale.
        self.renderer.invalidate_preview_cache();
        self.bump_version();
        Ok(())
    }

    /// An adjustment-layer slot is a mask: it must stay black-gray-white and
    /// fully opaque (white = full effect). After any op that writes the slot
    /// (paint, fill, shape, transform, delete) this re-imposes that invariant -
    /// fill sub-255 alpha by compositing over opaque white, then collapse each
    /// pixel to neutral gray. No-op for non-adjustment layers; does not
    /// recomposite (callers do that right after).
    fn normalize_adjustment_slot(&mut self, idx: usize) -> Result<(), RendererError> {
        if !self.layers.kind(idx).is_some_and(|k| k.is_adjustment()) {
            return Ok(());
        }
        let mut px = self.renderer.read_layer(idx)?;
        for p in px.chunks_exact_mut(4) {
            // Premultiplied BGRA OVER opaque white: c' = c + 255 * (1 - a). The
            // result is opaque, so the channels are now straight (un-premult).
            let inv = 255 - p[3];
            let b = f32::from(p[0].saturating_add(inv));
            let g = f32::from(p[1].saturating_add(inv));
            let r = f32::from(p[2].saturating_add(inv));
            // Rec. 709 luma -> neutral gray (BGRA byte order).
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = (0.0722 * b + 0.7152 * g + 0.2126 * r)
                .round()
                .clamp(0.0, 255.0) as u8;
            p[0] = v;
            p[1] = v;
            p[2] = v;
            p[3] = 255;
        }
        self.renderer.write_layer(idx, &px)?;
        Ok(())
    }

    /// Composite steps for a preview: folder-scoped when folders bound an
    /// adjustment, otherwise a flat list of the visible layers in canvas order.
    /// Always covers every visible layer.
    fn preview_steps(&self, snapshot: &[crate::document::Layer]) -> Vec<CompositeStep> {
        self.folder_scoped_steps(snapshot).unwrap_or_else(|| {
            snapshot
                .iter()
                .enumerate()
                .filter_map(|(i, l)| l.visible.then_some(CompositeStep::Layer(i)))
                .collect()
        })
    }

    /// Build the bracketed composite step stream when folder-scoped compositing
    /// is needed and possible: there is a folder tree, it has adjustment layers
    /// to scope, it actually contains a folder, and it matches the live stack.
    /// `None` falls back to the flat composite path.
    fn folder_scoped_steps(&self, snapshot: &[crate::document::Layer]) -> Option<Vec<CompositeStep>> {
        if self.layer_tree.is_empty() || !self.renderer.has_adjustment_layers() {
            return None;
        }
        let visible: Vec<usize> = snapshot
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.visible.then_some(i))
            .collect();
        let resolve = |id: &str| {
            snapshot
                .iter()
                .position(|l| l.id == id)
                .filter(|&i| snapshot[i].visible)
        };
        let steps = build_composite_steps(&self.layer_tree, &resolve, &visible);
        // No folders left after dropping hidden/empty ones -> flat is identical.
        steps
            .iter()
            .any(|s| matches!(s, CompositeStep::EnterGroup))
            .then_some(steps)
    }

    // ----------------------------------------------------------------
    // Display + readback
    // ----------------------------------------------------------------

    /// Forget every pattern slice previously uploaded to this canvas's
    /// atlas. Safe only when no stroke is in flight - the preview
    /// canvas calls this between renders so it doesn't fill up after
    /// 16 reloads of a textured brush (each reload mints a fresh
    /// `Rc<PatternData>` the atlas can't dedup against the old one).
    pub fn clear_pattern_atlas(&mut self) {
        self.renderer.clear_pattern_atlas();
    }

    /// Clear the active layer to `color`. No-op if no active layer.
    pub fn clear(&mut self, color: [f32; 4]) -> Result<(), RendererError> {
        let Some(idx) = self.layers.active() else {
            return Ok(());
        };
        self.renderer.clear_layer(idx, color)?;
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Read back the display dmabuf that [`Self::present`] last wrote, as BGRA8
    /// bytes (row-major, no padding). These are premultiplied *gamma*
    /// (`srgb(colour) * alpha`), not the premultiplied-linear form
    /// [`Self::read_pixels`] returns. Test/diagnostic helper - the live path
    /// hands this buffer to GTK rather than reading it back.
    pub fn read_display(&mut self) -> Result<Vec<u8>, RendererError> {
        self.renderer.read_display()
    }

    /// Read back the canvas as BGRA8 bytes (row-major, no padding).
    ///
    /// During a stroke this returns the preview (canvas composite +
    /// tinted in-flight stroke). Otherwise it returns the canvas
    /// composite directly. Byte order is BGRA so the bytes drop
    /// straight into `cairo::Format::ARgb32` and
    /// `gdk::MemoryFormat::B8g8r8a8` on little-endian.
    pub fn read_pixels(&mut self) -> Result<Vec<u8>, RendererError> {
        match self.current_stroke {
            Some(ctx) => {
                let linear = ctx.color.to_linear_rgb();
                let visibilities = self.visibilities();
                if self.painting_hidden_adjustment_mask(ctx.layer_idx) {
                    let snapshot = self.layers.snapshot();
                    let steps = self.preview_steps(&snapshot);
                    self.renderer.render_mask_edit_preview_and_read(
                        &steps,
                        ctx.layer_idx,
                        linear,
                        ctx.opacity,
                    )
                } else if let Some(steps) = self.folder_scoped_steps(&self.layers.snapshot()) {
                    // Folder-bounded adjustment: scope the effect to its folder
                    // (the flat path below would apply it to the whole backdrop).
                    self.renderer.render_preview_scoped_and_read(
                        &steps,
                        ctx.layer_idx,
                        linear,
                        ctx.opacity,
                    )
                } else if self.effective_adjustment_above(ctx.layer_idx) {
                    self.renderer.render_preview_adjusted_and_read(
                        &visibilities,
                        ctx.layer_idx,
                        linear,
                        ctx.opacity,
                    )
                } else {
                    self.renderer.render_preview_layered_and_read(
                        &visibilities,
                        ctx.layer_idx,
                        linear,
                        ctx.opacity,
                    )
                }
            }
            None => self.renderer.read_canvas(),
        }
    }

    /// Test/diagnostic: drive the incremental (dab-region-clipped) preview for
    /// the in-flight stroke and read the result. Returns the composited canvas
    /// when no stroke is active.
    pub fn read_incremental_preview(&mut self) -> Result<Vec<u8>, RendererError> {
        let Some(ctx) = self.current_stroke else {
            return self.renderer.read_canvas();
        };
        let linear = ctx.color.to_linear_rgb();
        let visibilities = self.visibilities();
        if self.effective_adjustment_above(ctx.layer_idx) {
            self.renderer.render_preview_adjusted_incremental_and_read(
                &visibilities,
                ctx.layer_idx,
                linear,
                ctx.opacity,
            )
        } else {
            self.renderer.render_preview_incremental_and_read(
                &visibilities,
                ctx.layer_idx,
                linear,
                ctx.opacity,
            )
        }
    }

    /// Test/diagnostic: force the next preview frame to rebuild the whole canvas
    /// (drops the incremental dab-region state).
    pub fn force_full_preview(&mut self) {
        self.renderer.invalidate_preview_cache();
    }

    /// Like [`Self::read_pixels`] but fills a caller-owned buffer instead
    /// of allocating a fresh `Vec` each call. Use this for repeated
    /// readbacks (e.g. layer-panel thumbnail refresh) to avoid churning a
    /// full-canvas allocation every time.
    pub fn read_pixels_into(&mut self, out: &mut Vec<u8>) -> Result<(), RendererError> {
        match self.current_stroke {
            Some(ctx) => {
                let linear = ctx.color.to_linear_rgb();
                let visibilities = self.visibilities();
                self.renderer.render_preview_layered_into(
                    &visibilities,
                    ctx.layer_idx,
                    linear,
                    ctx.opacity,
                    out,
                )
            }
            None => self.renderer.read_canvas_into(out),
        }
    }

    /// Like [`Self::read_layer`] but fills a caller-owned buffer.
    pub fn read_layer_into(&mut self, idx: usize, out: &mut Vec<u8>) -> Result<(), RendererError> {
        self.renderer.read_layer_into(idx, out)
    }

    /// Prepare the live transform preview's z-order. Reads back the visible
    /// layers above `target_idx` into `out` (BGRA8, the overlay the UI draws on
    /// top of the preview) and rebuilds the base canvas from only the layers up
    /// to and including `target_idx`, so the upper layers aren't composited
    /// twice. Any layer-mutating path (restore/apply) rebuilds the full canvas
    /// again when the transform ends.
    pub fn begin_transform_preview(
        &mut self,
        target_idx: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), RendererError> {
        let visibilities = self.visibilities();
        self.renderer.read_layers_above(&visibilities, target_idx, out)?;
        self.renderer
            .composite_layers_below_to_canvas(&visibilities, target_idx)?;
        self.bump_version();
        Ok(())
    }

    /// Read a `w x h` sub-rectangle of layer `idx` at `(x, y)` into `out`
    /// (BGRA8, tightly packed). Used to capture just a stroke's dirty
    /// region for history instead of the whole layer.
    pub fn read_layer_region_into(
        &mut self,
        idx: usize,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), RendererError> {
        self.renderer.read_layer_region_into(idx, x, y, w, h, out)
    }

    /// Tight integer AABB `(x, y, w, h)` of everything stamped since the
    /// last `begin_stroke`, clamped to the canvas. `None` if nothing was
    /// painted. Stays valid across `commit_stroke` (reset only by the next
    /// `begin_stroke`), so history can read it after committing.
    #[must_use]
    pub fn stroke_dirty_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        self.renderer.stroke_dirty_bounds()
    }

    fn visibilities(&self) -> Vec<bool> {
        self.layers.snapshot().iter().map(|l| l.visible).collect()
    }

    /// Non-blocking GPU timings of the most recent drawing frame, in
    /// milliseconds: `(render_ms, present_ms)`. `None` until results are ready
    /// or if the device lacks timestamp queries. For the perf overlay.
    #[must_use]
    pub fn frame_timings(&self) -> Option<(f32, f32)> {
        self.renderer.poll_frame_timings()
    }

    /// Zero-copy display path. Ensures the dmabuf display image is up
    /// to date for the current state (compositing the in-flight stroke
    /// on top of the canvas during a stroke, or the fill overlay
    /// during a bucket-fill animation) and returns the descriptor GTK
    /// uses to import it.
    pub fn present(&mut self) -> Result<DmabufDescriptor, RendererError> {
        use crate::renderer::PresentSource;
        // When an adjustment layer is the active selection (and nothing else is
        // in flight), the canvas shows its grayscale mask so it can be edited.
        let want_mask = self.mask_view_idx();
        let dirty =
            self.display_version != self.pixels_version || want_mask != self.displayed_mask_idx;
        if dirty {
            if self.renderer.filter_active() {
                let visibilities = self.visibilities();
                // Folder-bounded / global adjustment around a single filtered
                // layer: the flat preview ignores adjustment slots and folder
                // scope, so the adjustment would bleed onto the whole canvas
                // (unclipped) until the filter is applied. Route through the
                // scoped walk so the live preview clips like the committed result.
                match self.renderer.filter_single_target() {
                    Some(target) if self.effective_adjustment_excluding(target) => {
                        let snapshot = self.layers.snapshot();
                        let steps = self.preview_steps(&snapshot);
                        self.renderer.render_filter_preview_scoped(&steps, target)?;
                    }
                    _ => {
                        self.renderer.render_filter_preview(&visibilities)?;
                    }
                }
                self.renderer.present_to_display(PresentSource::Preview)?;
            } else if self.renderer.fill_active() {
                let visibilities = self.visibilities();
                self.renderer.render_fill_preview(&visibilities)?;
                self.renderer.present_to_display(PresentSource::Preview)?;
            } else if self.renderer.shape_active() {
                let visibilities = self.visibilities();
                self.renderer.render_shape_preview(&visibilities)?;
                self.renderer.present_to_display(PresentSource::Preview)?;
            } else if self.renderer.gradient_active() {
                let visibilities = self.visibilities();
                // Folder-bounded adjustment around the gradient's target: the flat
                // preview skips adjustment slots and folder scope, so it would show
                // the ramp unadjusted (bright) until commit. Route through the
                // scoped walk so the live preview clips like the committed result.
                let target = self.renderer.gradient_target();
                if self.effective_adjustment_excluding(target) {
                    let snapshot = self.layers.snapshot();
                    let steps = self.preview_steps(&snapshot);
                    self.renderer.render_gradient_preview_scoped(&steps, target)?;
                } else {
                    self.renderer.render_gradient_preview(&visibilities)?;
                }
                self.renderer.present_to_display(PresentSource::Preview)?;
            } else if self.renderer.transform_preview_active() {
                let visibilities = self.visibilities();
                // Run the adjustment chain (folder-scoped) over the transform
                // preview whenever any effective adjustment is in play - above the
                // target (the warped layer must preview adjusted) or below it (the
                // fast path skips adjustment slots, so it would drop the effect on
                // the static layers under the one being transformed).
                let target = self.renderer.transform_preview_target();
                if target.is_some_and(|t| self.effective_adjustment_excluding(t)) {
                    let snapshot = self.layers.snapshot();
                    let steps = self.preview_steps(&snapshot);
                    self.renderer.render_transform_preview_scoped(&steps, &visibilities)?;
                } else {
                    self.renderer.render_transform_preview(&visibilities)?;
                }
                self.renderer.present_to_display(PresentSource::Preview)?;
            } else {
                match self.current_stroke {
                    Some(ctx) => {
                        let linear = ctx.color.to_linear_rgb();
                        let visibilities = self.visibilities();
                        // Painting an adjustment layer's mask with the grayscale
                        // mask view OFF: preview the effect gated by the live
                        // (committed + in-flight) mask, never the mask itself.
                        if self.painting_hidden_adjustment_mask(ctx.layer_idx) {
                            let snapshot = self.layers.snapshot();
                            let steps = self.preview_steps(&snapshot);
                            self.renderer.render_mask_edit_preview_and_present(
                                &steps,
                                ctx.layer_idx,
                                linear,
                                ctx.opacity,
                            )?;
                        }
                        // Folder-bounded adjustment: the flat fast path can't clip
                        // the effect to its folder (it would bleed onto every layer
                        // below). The scoped path caches static folders per stroke,
                        // so it stays cheap even when painting outside that folder.
                        else if let Some(steps) =
                            self.folder_scoped_steps(&self.layers.snapshot())
                        {
                            self.renderer.render_preview_scoped_and_present(
                                &steps,
                                ctx.layer_idx,
                                linear,
                                ctx.opacity,
                            )?;
                        }
                        // Live effect preview: when an effective (non-folder)
                        // adjustment sits above the painted layer, composite the
                        // in-flight stroke through the effect chain so the canvas
                        // shows the adjusted result while drawing.
                        else if self.effective_adjustment_above(ctx.layer_idx) {
                            self.renderer.render_preview_adjusted_and_present(
                                &visibilities,
                                ctx.layer_idx,
                                linear,
                                ctx.opacity,
                            )?;
                        } else {
                            self.renderer.render_preview_and_present(
                                &visibilities,
                                ctx.layer_idx,
                                linear,
                                ctx.opacity,
                            )?;
                        }
                    }
                    None => {
                        if let Some(idx) = want_mask {
                            self.renderer.render_mask_preview(idx)?;
                            self.renderer.present_to_display(PresentSource::Preview)?;
                        } else {
                            self.renderer.present_to_display(PresentSource::Canvas)?;
                        }
                    }
                }
            }
            self.display_version = self.pixels_version;
            self.displayed_mask_idx = want_mask;
            // Block until the present copy actually finishes on the GPU before we
            // hand the dmabuf to GTK. The copy is async-submitted, so otherwise we
            // give GTK a descriptor for a buffer whose write is still in flight;
            // GTK's compositor read then stalls on the implicit dma-buf fence,
            // which (on Wayland) withholds the frame callback AND the frame-clock-
            // gated stylus events - freezing input for 100-300ms. Waiting here
            // (a few tens of us) moves that sync onto our idle main thread, the
            // GPU-renderer equivalent of what GSK_RENDERER=cairo does by reading
            // the buffer back to the CPU.
            self.renderer.wait_last()?;
        }
        Ok(self.renderer.display_descriptor())
    }

    /// The adjustment layer whose mask should be shown on the canvas right now:
    /// the one the user toggled into mask view, when no stroke is in flight (a
    /// stroke on the adjustment renders the mask + the live dab instead).
    fn mask_view_idx(&self) -> Option<usize> {
        if self.current_stroke.is_some() {
            return None;
        }
        let id = self.mask_view_id.as_deref()?;
        let idx = self.layers.snapshot().iter().position(|l| l.id == id)?;
        self.layer_is_adjustment(idx).then_some(idx)
    }

    /// Id of the adjustment layer whose mask is toggled into view, if any.
    #[must_use]
    pub fn mask_view(&self) -> Option<&str> {
        self.mask_view_id.as_deref()
    }

    /// Show (`Some(layer_id)`) or hide (`None`) an adjustment layer's mask on the
    /// canvas. Only takes visible effect for ids that name an adjustment layer.
    pub fn set_mask_view(&mut self, id: Option<String>) {
        if self.mask_view_id != id {
            self.mask_view_id = id;
            self.bump_version();
        }
    }

    fn layer_is_adjustment(&self, idx: usize) -> bool {
        matches!(self.layers.kind(idx), Some(LayerKind::Adjustment(_)))
    }

    /// `true` when the in-flight stroke is painting an adjustment layer's mask
    /// AND that layer's grayscale mask is NOT toggled into view. In that case
    /// the canvas should preview the effect with the live mask, not show the
    /// black/white mask. With the mask view ON, the user wants to see the mask
    /// being painted, so this stays false (the normal stroked-target preview
    /// renders the mask + the dab).
    fn painting_hidden_adjustment_mask(&self, idx: usize) -> bool {
        if !self.layer_is_adjustment(idx) {
            return false;
        }
        let snapshot = self.layers.snapshot();
        let Some(this_id) = snapshot.get(idx).map(|l| l.id.as_str()) else {
            return false;
        };
        self.mask_view_id.as_deref() != Some(this_id)
    }

    /// `true` when a visible adjustment layer with a non-empty effect stack sits
    /// *above* `target` (higher z-index). Only then does a stroke on `target`
    /// need the slow per-frame adjusted preview - the effect reprocesses the
    /// changing stroke every frame. Strokes that no effective adjustment
    /// influences keep the fast cached preview path.
    fn effective_adjustment_above(&self, target: usize) -> bool {
        self.layers
            .snapshot()
            .iter()
            .enumerate()
            .any(|(idx, l)| {
                idx > target
                    && l.visible
                    && matches!(&l.kind, LayerKind::Adjustment(d) if !d.is_noop())
            })
    }

    /// `true` when a visible effective adjustment layer other than `target`
    /// exists anywhere in the stack. The transform preview's fast path skips
    /// adjustment slots, so it needs the adjustment-aware (scoped) path when an
    /// adjustment sits either above the transformed layer (the warp must preview
    /// adjusted) or below it (the static layers under the adjustment must stay
    /// adjusted). The target itself is excluded - its content is warped, not
    /// applied as an effect.
    fn effective_adjustment_excluding(&self, target: usize) -> bool {
        self.layers
            .snapshot()
            .iter()
            .enumerate()
            .any(|(idx, l)| {
                idx != target
                    && l.visible
                    && matches!(&l.kind, LayerKind::Adjustment(d) if !d.is_noop())
            })
    }

    /// Fast per-motion-event path: stamp the brush dabs AND refresh the
    /// dmabuf display in a single GPU submit (one fence-wait instead of a
    /// separate `stamp` + `present`). Falls back to `stamp` + `present`
    /// when there is no in-flight stroke or a filter/fill overlay is
    /// active. Returns the descriptor GTK imports.
    pub fn stamp_and_present<F>(&mut self, paint: F) -> Result<DmabufDescriptor, RendererError>
    where
        F: FnOnce(&mut dyn PaintTarget),
    {
        let Some(ctx) = self.current_stroke else {
            self.stamp(paint)?;
            return self.present();
        };
        // The fast single-submit preview can't run effect chains, so when an
        // effective adjustment above this layer would alter the stroke's result
        // fall back to the slower stamp + present (the adjusted preview path).
        // Strokes no adjustment influences keep the fast path - no slowdown.
        let needs_adjusted_preview = self.effective_adjustment_above(ctx.layer_idx)
            || self.painting_hidden_adjustment_mask(ctx.layer_idx)
            || self.folder_scoped_steps(&self.layers.snapshot()).is_some();
        if self.renderer.filter_active()
            || self.renderer.fill_active()
            || self.renderer.shape_active()
            || self.renderer.gradient_active()
            || needs_adjusted_preview
        {
            self.stamp(paint)?;
            return self.present();
        }

        let (family, instances) = {
            let mut batch = BatchStamp::new(&mut self.renderer);
            paint(&mut batch);
            batch.into_result()?
        };
        let linear = ctx.color.to_linear_rgb();
        let visibilities = self.visibilities();
        self.renderer.stamp_preview_present(
            family,
            &instances,
            ctx.layer_idx,
            linear,
            ctx.opacity,
            &visibilities,
        )?;
        self.bump_version();
        self.display_version = self.pixels_version;
        Ok(self.renderer.display_descriptor())
    }

    /// Whether the dmabuf display image is currently out of date.
    #[must_use]
    pub const fn display_dirty(&self) -> bool {
        self.pixels_version != self.display_version
    }

    /// Whether a stroke is currently in flight (between `begin_stroke`
    /// and `commit_stroke`/`discard_stroke`).
    #[must_use]
    pub const fn is_drawing(&self) -> bool {
        self.current_stroke.is_some()
    }

    /// Read back a single layer's pixels as BGRA8 bytes (row-major, no
    /// padding). Caller should check `is_drawing()` first; reading
    /// during a stroke returns the layer's committed pixels, not the
    /// in-flight stroke.
    pub fn read_layer(&mut self, idx: usize) -> Result<Vec<u8>, RendererError> {
        self.renderer.read_layer(idx)
    }

    /// Sample the visible (composited) color at canvas pixel `(x, y)`.
    /// Returns `None` for out-of-bounds coordinates. The composite is
    /// premultiplied, so the stored RGB is un-premultiplied back to its
    /// straight color before returning; a fully transparent pixel yields
    /// black.
    pub fn pick_color(&mut self, x: u32, y: u32) -> Option<Color> {
        let size = self.size();
        if x >= size.width || y >= size.height {
            return None;
        }
        let mut buf = Vec::with_capacity(4);
        self.renderer.read_canvas_region_into(x, y, 1, 1, &mut buf).ok()?;
        let [b, g, r, a] = [*buf.first()?, *buf.get(1)?, *buf.get(2)?, *buf.get(3)?];
        if a == 0 {
            return Some(Color::BLACK);
        }
        // Un-premultiply: straight = premult * 255 / alpha, clamped.
        let unpremult = |c: u8| -> u8 {
            ((u16::from(c) * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8
        };
        Some(Color::new(unpremult(r), unpremult(g), unpremult(b)))
    }

    /// Crop the canvas to the given rectangle and resize. Reads every layer's
    /// pixels from the GPU, crops them, recreates the renderer at the new size,
    /// and writes the cropped pixels back. Returns the new canvas size.
    ///
    /// The crop rectangle may extend beyond the current canvas - out-of-bounds
    /// regions are filled with transparent pixels. This means passing a rect
    /// with a negative origin or a size larger than the canvas effectively
    /// expands the canvas.
    pub fn apply_crop(&mut self, rect: CropRect) -> Result<Size, RendererError> {
        let saved_active = self.layers.active();

        let n = rect.normalized();
        let crop_x = n.x.round() as i64;
        let crop_y = n.y.round() as i64;
        let w = n.w.round().max(1.0) as u32;
        let h = n.h.round().max(1.0) as u32;
        let new_size = Size::new(w, h);
        let old_size = self.renderer.canvas_size();

        let snap = self.layers.snapshot();
        let mut cropped: Vec<(String, String, bool, BlendMode, f32, Vec<u8>)> =
            Vec::with_capacity(snap.len());
        let kinds: Vec<LayerKind> = snap.iter().map(|l| l.kind.clone()).collect();
        for (idx, layer) in snap.iter().enumerate() {
            let raw = self.renderer.read_layer(idx)?;
            let pixels = crop_bgra8(&raw, old_size.width, old_size.height, crop_x, crop_y, w, h);
            cropped.push((
                layer.id.clone(),
                layer.name.clone(),
                layer.visible,
                layer.blend,
                layer.opacity,
                pixels,
            ));
        }

        self.renderer = VulkanRenderer::new(new_size)?;
        self.current_stroke = None;
        self.replace_all_layers(&cropped)?;

        // replace_all_layers resets every layer to Raster; restore the original
        // kinds with their text/component geometry shifted by the crop offset so
        // boxes stay aligned with their now-translated pixels.
        #[allow(clippy::cast_precision_loss)]
        let (dx, dy) = (-(crop_x as f32), -(crop_y as f32));
        for (idx, kind) in kinds.iter().enumerate() {
            if !matches!(kind, LayerKind::Raster) {
                self.layers.set_kind(idx, kind.translated(dx, dy));
            }
        }

        // replace_all_layers always resets active to Some(0); restore the
        // caller's active selection so downstream operations target the right layer.
        if let Some(active) = saved_active
            && active < self.layers.len() {
                self.layers.set_active(Some(active));
            }

        Ok(new_size)
    }

    /// Recreate the renderer at `size` and load a fresh layer set. Used to
    /// swap the canvas between the main document and a component's edit
    /// surface (which may have a different size). `layers` are
    /// `(id, name, visible, blend, opacity, bgra8_pixels)` sized to `size`;
    /// `active` selects the active layer afterwards. Discards any in-flight
    /// stroke.
    pub fn resize_and_replace_layers(
        &mut self,
        size: Size,
        layers: &[(String, String, bool, BlendMode, f32, Vec<u8>)],
        active: Option<usize>,
    ) -> Result<(), RendererError> {
        self.renderer = VulkanRenderer::new(size)?;
        self.current_stroke = None;
        self.replace_all_layers(layers)?;
        if let Some(a) = active
            && a < self.layers.len()
        {
            self.layers.set_active(Some(a));
        }
        Ok(())
    }

    /// Write raw BGRA8 pixels back to a GPU layer and re-composite.
    /// Used by the transform cancel path to restore pixels without CPU remap.
    pub fn restore_layer(&mut self, idx: usize, pixels: &[u8]) -> Result<(), RendererError> {
        self.renderer.write_layer(idx, pixels)?;
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Like [`Self::restore_layer`] but uploads only the `w x h` sub-rect at
    /// `(x, y)` (clamped to the canvas); the rest of the layer is untouched.
    /// `pixels` is tightly packed BGRA8 for the region. Used by the text editor
    /// so a keystroke uploads just the box region instead of the whole canvas.
    pub fn restore_layer_region(
        &mut self,
        idx: usize,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        pixels: &[u8],
    ) -> Result<(), RendererError> {
        self.renderer.write_layer_region(idx, x, y, w, h, pixels)?;
        self.recomposite_canvas()?;
        Ok(())
    }

    // ----------------------------------------------------------------
    // Bucket-fill GPU overlay
    // ----------------------------------------------------------------

    /// Begin a bucket-fill animation. Uploads the R8 distance mask
    /// produced by `flood_fill` to the overlay image (one-shot
    /// transfer, big on huge canvases) and arms the overlay path so
    /// subsequent `present()` calls render the canvas + fill colour
    /// clipped to the reveal radius. Bump `pixels_version` so the next
    /// present is non-trivial.
    pub fn begin_fill_overlay(
        &mut self,
        layer_idx: usize,
        distance_mask: &[u8],
        color: Color,
    ) -> Result<(), RendererError> {
        // Adjustment masks are grayscale: match the committed (normalized) result.
        let color = if self.layers.kind(layer_idx).is_some_and(|k| k.is_adjustment()) {
            color.to_grayscale()
        } else {
            color
        };
        let linear = color.to_linear_rgb();
        // Premultiplied with alpha = 1.0 since bucket fill is opaque;
        // the OVER blend on the overlay pipeline matches this.
        let color_premul = [linear[0], linear[1], linear[2], 1.0];
        self.renderer.upload_fill_mask(distance_mask)?;
        self.renderer.begin_fill_overlay(layer_idx, color_premul)?;
        self.bump_version();
        Ok(())
    }

    /// Update the reveal radius (0.0..=1.0 in normalised mask space)
    /// and mark the display dirty so the next present picks it up.
    pub const fn set_fill_reveal(&mut self, reveal: f32) {
        self.renderer.set_fill_reveal(reveal);
        self.bump_version();
    }

    /// Whether a fill overlay is currently active.
    #[must_use]
    pub const fn fill_overlay_active(&self) -> bool {
        self.renderer.fill_active()
    }

    /// Commit the final filled pixels to the target layer and clear
    /// the overlay. Returns the layer back to normal composition.
    pub fn commit_fill_overlay(
        &mut self,
        layer_idx: usize,
        pixels: &[u8],
    ) -> Result<(), RendererError> {
        self.renderer.write_layer(layer_idx, pixels)?;
        self.renderer.clear_fill_overlay();
        self.normalize_adjustment_slot(layer_idx)?;
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Cancel an in-flight fill overlay without committing anything.
    pub const fn cancel_fill_overlay(&mut self) {
        self.renderer.clear_fill_overlay();
        self.bump_version();
    }

    // ----------------------------------------------------------------
    // Shape tool GPU overlay
    // ----------------------------------------------------------------

    /// Arm the GPU shape overlay for a drag on `layer_idx`. Subsequent
    /// `set_shape_preview_params` + `present()` calls render the shape
    /// directly into the preview image - no CPU rasterisation or
    /// texture upload per frame.
    pub fn begin_shape_overlay(&mut self, layer_idx: usize) {
        self.renderer.begin_shape_overlay(layer_idx);
        self.bump_version();
    }

    /// Update the in-flight shape's parameters. `color` is converted to
    /// premultiplied linear RGB before being pushed.
    ///
    /// `rect` is `(x, y, w, h)` for box shapes; `(x0, y0, x1, y1)` for
    /// `ShapeKind::Line`. `line_width` is only consulted for Line.
    pub fn set_shape_preview_params(
        &mut self,
        kind: ShapeKind,
        rect: [f32; 4],
        color: Color,
        antialias: bool,
        line_width: f32,
    ) {
        let linear = color.to_linear_rgb();
        let premul = [linear[0], linear[1], linear[2], 1.0];
        self.renderer
            .set_shape_preview_params(kind, rect, premul, antialias, line_width);
        self.bump_version();
    }

    /// Whether a shape overlay is currently active.
    #[must_use]
    pub const fn shape_overlay_active(&self) -> bool {
        self.renderer.shape_active()
    }

    /// Commit the shape into the target layer (GPU OVER blend), clear
    /// the overlay, and recomposite the canvas.
    pub fn commit_shape(
        &mut self,
        layer_idx: usize,
        kind: ShapeKind,
        rect: [f32; 4],
        color: Color,
        antialias: bool,
        line_width: f32,
    ) -> Result<(), RendererError> {
        let linear = color.to_linear_rgb();
        let premul = [linear[0], linear[1], linear[2], 1.0];
        self.renderer
            .commit_shape(layer_idx, kind, rect, premul, antialias, line_width)?;
        self.normalize_adjustment_slot(layer_idx)?;
        self.recomposite_canvas()
    }

    /// Cancel an in-flight shape overlay without committing.
    pub fn cancel_shape_overlay(&mut self) {
        self.renderer.clear_shape_overlay();
        self.bump_version();
    }

    // ----------------------------------------------------------------
    // Gradient tool GPU overlay
    // ----------------------------------------------------------------

    /// Arm the GPU gradient overlay for a drag on `layer_idx`. Upload the
    /// LUT once with `set_gradient_lut`, then push endpoints per drag move.
    pub fn begin_gradient_overlay(&mut self, layer_idx: usize) {
        self.renderer.begin_gradient_overlay(layer_idx);
        self.bump_version();
    }

    /// Upload the baked ramp LUT (premultiplied linear RGBA, one entry per
    /// `GRADIENT_LUT_SIZE` step).
    pub fn set_gradient_lut(&mut self, lut: &[f32]) -> Result<(), RendererError> {
        self.renderer.set_gradient_lut(lut)?;
        self.bump_version();
        Ok(())
    }

    /// Update the in-flight gradient's geometry. `endpoints` is
    /// `(x0, y0, x1, y1)` in canvas pixels.
    pub fn set_gradient_preview_params(&mut self, kind: GradientKind, endpoints: [f32; 4]) {
        self.renderer.set_gradient_preview_params(kind, endpoints);
        self.bump_version();
    }

    /// Whether a gradient overlay is currently active.
    #[must_use]
    pub const fn gradient_overlay_active(&self) -> bool {
        self.renderer.gradient_active()
    }

    /// Commit the gradient into the target layer (GPU OVER blend), clear
    /// the overlay, and recomposite the canvas.
    pub fn commit_gradient(
        &mut self,
        layer_idx: usize,
        kind: GradientKind,
        endpoints: [f32; 4],
    ) -> Result<(), RendererError> {
        self.renderer.commit_gradient(layer_idx, kind, endpoints)?;
        self.normalize_adjustment_slot(layer_idx)?;
        self.recomposite_canvas()
    }

    /// Cancel an in-flight gradient overlay without committing.
    pub fn cancel_gradient_overlay(&mut self) {
        self.renderer.clear_gradient_overlay();
        self.bump_version();
    }

    // ----------------------------------------------------------------
    // Filters (HSV / invert / blur / sharpen) - GPU live preview + apply
    // ----------------------------------------------------------------

    /// Arm the filter live-preview path. `indices` are the layers the filter
    /// applies to; `spec` is the initial parameters. The layer images are
    /// left untouched - `present` re-renders the preview through the filter
    /// pipeline - so [`Self::cancel_filter`] is a clean no-op.
    pub fn begin_filter(&mut self, indices: &[usize], spec: FilterSpec) {
        self.renderer.begin_filter(indices.to_vec(), spec);
        self.bump_version();
    }

    /// Update the previewed filter parameters (slider moved).
    pub const fn update_filter(&mut self, spec: FilterSpec) {
        self.renderer.update_filter_spec(spec);
        self.bump_version();
    }

    /// Whether a filter preview is currently armed.
    #[must_use]
    pub const fn filter_active(&self) -> bool {
        self.renderer.filter_active()
    }

    /// Commit the filter to every armed layer, writing the filtered pixels
    /// into the layer images and re-compositing. History is captured by the
    /// caller via `read_layer` before/after.
    pub fn apply_filter(&mut self, indices: &[usize], spec: FilterSpec) -> Result<(), RendererError> {
        for &idx in indices {
            self.renderer.apply_filter_to_layer(idx, spec)?;
        }
        self.renderer.clear_filter();
        self.recomposite_canvas()?;
        self.bump_version();
        Ok(())
    }

    /// Render the armed filter preview and read it back as BGRA8. Intended
    /// for tests/diagnostics; the live path presents straight to the display.
    pub fn read_filter_preview(&mut self) -> Result<Vec<u8>, RendererError> {
        match self.renderer.filter_single_target() {
            Some(target) if self.effective_adjustment_excluding(target) => {
                let snapshot = self.layers.snapshot();
                let steps = self.preview_steps(&snapshot);
                self.renderer.read_filter_preview_scoped(&steps, target)
            }
            _ => {
                let vis = self.visibilities();
                self.renderer.read_filter_preview(&vis)
            }
        }
    }

    /// Cancel an in-flight filter preview. Layer images were never modified,
    /// so this only disarms the preview path.
    pub fn cancel_filter(&mut self) {
        self.renderer.clear_filter();
        self.bump_version();
    }

    /// Clear a specific layer (by index) to `color` and re-composite.
    pub fn clear_layer_at(&mut self, idx: usize, color: [f32; 4]) -> Result<(), RendererError> {
        self.renderer.clear_layer(idx, color)?;
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Apply an affine transform to a single layer. `original` must be the
    /// layer's BGRA8 pixels captured before the transform began (`src_w x
    /// src_h`, row-major, no padding). `src_w`/`src_h` are the dimensions of
    /// `original` - they may differ from the current canvas size when the
    /// canvas was expanded after the transform was initiated. `original_rect`
    /// is the tight bounding-box of non-transparent content in `original`
    /// coordinates; `current_rect` is the live transform rect in the current
    /// canvas coordinate space. Remaps `original_rect` -> `current_rect`.
    /// Writes the transformed result back to the GPU layer and re-composites.
    pub fn apply_layer_transform(
        &mut self,
        layer_idx: usize,
        original: &[u8],
        src_w: u32,
        src_h: u32,
        original_rect: TransformRect,
        current_rect: TransformRect,
        filter: TransformFilter,
    ) -> Result<(), RendererError> {
        let size = self.renderer.canvas_size();
        let transformed = transform_bgra8(
            original,
            src_w,
            src_h,
            size.width,
            size.height,
            original_rect,
            current_rect,
            filter,
        );
        self.renderer.write_layer(layer_idx, &transformed)?;
        self.normalize_adjustment_slot(layer_idx)?;
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Run the affine transform entirely in CPU and return the output buffer
    /// without writing to the GPU layer. The caller controls the output size,
    /// allowing buffers larger or smaller than the canvas.
    pub fn compute_transform_pixels(
        original: &[u8],
        src_w: u32,
        src_h: u32,
        out_w: u32,
        out_h: u32,
        original_rect: TransformRect,
        current_rect: TransformRect,
        filter: TransformFilter,
    ) -> Vec<u8> {
        transform_bgra8(
            original,
            src_w,
            src_h,
            out_w,
            out_h,
            original_rect,
            current_rect,
            filter,
        )
    }

    /// GPU transform apply. Renders the inverse affine through a Vulkan
    /// graphics pipeline, blits the canvas-overlap directly into the active
    /// layer image (GPU->GPU), and returns the full AABB pixels (for the
    /// extension store) plus the AABB metadata so the caller can build a
    /// `LayerExtension` without recomputing the bounding box.
    ///
    /// Returns `(pixels, ext_x, ext_y, out_w, out_h)`.
    pub fn apply_layer_transform_gpu(
        &mut self,
        layer_idx: usize,
        source_pixels: &[u8],
        src_w: u32,
        src_h: u32,
        original_rect: TransformRect,
        current_rect: TransformRect,
    ) -> Result<(Vec<u8>, i32, i32, u32, u32), RendererError> {
        // AABB of the rotated/scaled current_rect, snapped to pixel boundaries.
        let (hw, hh) = (current_rect.half_w(), current_rect.half_h());
        let corners = [
            current_rect.local_to_canvas(-hw, -hh),
            current_rect.local_to_canvas(hw, -hh),
            current_rect.local_to_canvas(-hw, hh),
            current_rect.local_to_canvas(hw, hh),
        ];
        let min_x = corners
            .iter()
            .map(|&(x, _)| x)
            .fold(f32::INFINITY, f32::min);
        let min_y = corners
            .iter()
            .map(|&(_, y)| y)
            .fold(f32::INFINITY, f32::min);
        let max_x = corners
            .iter()
            .map(|&(x, _)| x)
            .fold(f32::NEG_INFINITY, f32::max);
        let max_y = corners
            .iter()
            .map(|&(_, y)| y)
            .fold(f32::NEG_INFINITY, f32::max);
        #[allow(clippy::cast_possible_truncation)]
        let ext_x = min_x.floor() as i32;
        #[allow(clippy::cast_possible_truncation)]
        let ext_y = min_y.floor() as i32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let out_w = (max_x.ceil() as i32 - ext_x).max(1) as u32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let out_h = (max_y.ceil() as i32 - ext_y).max(1) as u32;

        let push = compute_transform_push(
            original_rect,
            current_rect,
            src_w,
            src_h,
            out_w,
            out_h,
            ext_x,
            ext_y,
        );

        let pixels = self.renderer.apply_layer_transform_gpu(
            layer_idx,
            source_pixels,
            src_w,
            src_h,
            out_w,
            out_h,
            ext_x,
            ext_y,
            push,
        )?;
        self.normalize_adjustment_slot(layer_idx)?;
        self.recomposite_canvas()?;
        Ok((pixels, ext_x, ext_y, out_w, out_h))
    }

    /// Start the live GPU transform preview for `layer_idx`. `source_pixels`
    /// are the upright `src_w x src_h` pixels being transformed (already lifted
    /// out of the layer, which the caller has cleared). The caller then drives
    /// it per drag-frame with [`Self::set_transform_preview`].
    pub fn begin_transform_preview_gpu(
        &mut self,
        layer_idx: usize,
        source_pixels: &[u8],
        src_w: u32,
        src_h: u32,
    ) -> Result<(), RendererError> {
        self.renderer
            .begin_transform_preview_gpu(layer_idx, source_pixels, src_w, src_h)?;
        self.bump_version();
        Ok(())
    }

    /// Update the live transform preview to `current_rect` and refresh the
    /// display. No-op if no preview session is active.
    pub fn set_transform_preview(
        &mut self,
        original_rect: TransformRect,
        current_rect: TransformRect,
        src_w: u32,
        src_h: u32,
    ) {
        let cs = self.renderer.canvas_size();
        let push = compute_transform_push(
            original_rect,
            current_rect,
            src_w,
            src_h,
            cs.width,
            cs.height,
            0,
            0,
        );
        self.renderer.set_transform_preview_push(push);
        self.bump_version();
    }

    /// Whether the live GPU transform preview is active.
    #[must_use]
    pub fn transform_preview_active(&self) -> bool {
        self.renderer.transform_preview_active()
    }

    /// Render the live transform preview and read it back as BGRA8. Diagnostic /
    /// test helper; the interactive path presents it instead.
    pub fn read_transform_preview(&mut self) -> Result<Vec<u8>, RendererError> {
        let visibilities = self.visibilities();
        let target = self.renderer.transform_preview_target();
        if target.is_some_and(|t| self.effective_adjustment_above(t)) {
            let snapshot = self.layers.snapshot();
            let steps = self.preview_steps(&snapshot);
            self.renderer.read_transform_preview_scoped(&steps, &visibilities)
        } else {
            self.renderer.read_transform_preview(&visibilities)
        }
    }

    /// Tear down the live transform preview (on apply/cancel/tool-switch).
    /// No-op (and no version bump) when no preview session is active.
    pub fn clear_transform_preview(&mut self) {
        if self.renderer.transform_preview_active() {
            self.renderer.clear_transform_preview_gpu();
            self.bump_version();
        }
    }
}

/// Precompute the 2x3 inverse affine that maps an output framebuffer's `[0,1]`
/// UV directly to the source texture's `[0,1]` UV. See the shader comment in
/// `transform.frag`; the chain is:
///   v_uv -> output_px -> canvas_px -> current_local -> source_local ->
///   source_px -> source_uv
/// collapsed to two row-vec3 dot products. `out_w/out_h/ext_x/ext_y` describe
/// the output framebuffer (the AABB for a commit, or the full canvas at origin
/// for the live preview).
#[allow(clippy::too_many_arguments)]
fn compute_transform_push(
    original_rect: TransformRect,
    current_rect: TransformRect,
    src_w: u32,
    src_h: u32,
    out_w: u32,
    out_h: u32,
    ext_x: i32,
    ext_y: i32,
) -> [f32; 8] {
    let ca = current_rect.angle.cos();
    let sa = current_rect.angle.sin();
    let kx = original_rect.w / current_rect.w;
    let ky = original_rect.h / current_rect.h;
    #[allow(clippy::cast_precision_loss)]
    let ow = out_w as f32;
    #[allow(clippy::cast_precision_loss)]
    let oh = out_h as f32;
    #[allow(clippy::cast_precision_loss)]
    let ox = ext_x as f32;
    #[allow(clippy::cast_precision_loss)]
    let oy = ext_y as f32;
    let ccx = current_rect.cx;
    let ccy = current_rect.cy;
    let ocx = original_rect.cx;
    let ocy = original_rect.cy;
    #[allow(clippy::cast_precision_loss)]
    let sw = src_w as f32;
    #[allow(clippy::cast_precision_loss)]
    let sh = src_h as f32;

    let a1 = ow * ca;
    let b1 = oh * sa;
    let c1 = (ox - ccx).mul_add(ca, (oy - ccy) * sa);
    let a2 = -ow * sa;
    let b2 = oh * ca;
    let c2 = (-(ox - ccx)).mul_add(sa, (oy - ccy) * ca);

    [
        a1 * kx / sw,
        b1 * kx / sw,
        c1.mul_add(kx, ocx) / sw,
        0.0,
        a2 * ky / sh,
        b2 * ky / sh,
        c2.mul_add(ky, ocy) / sh,
        0.0,
    ]
}

impl Canvas {
    // ----------------------------------------------------------------
    // Selection
    // ----------------------------------------------------------------

    /// Fill the entire mask. After this, `selection_active()` is true.
    pub fn select_all(&mut self) -> Result<(), RendererError> {
        self.renderer.select_all()?;
        self.bump_version();
        Ok(())
    }

    /// Mark the mask as inert. The mask's contents are left in place
    /// (they're don't-care while inactive) - the composite shader's
    /// `selection_active` push constant gates whether they apply.
    pub const fn deselect(&mut self) {
        self.renderer.deselect();
        self.bump_version();
    }

    /// Logical NOT of the current mask. No-op when no selection is active.
    pub fn invert_selection(&mut self) -> Result<(), RendererError> {
        self.renderer.invert_selection()?;
        self.bump_version();
        Ok(())
    }

    /// Rasterise `shape` into an R8 buffer and blend it into the mask
    /// with the GPU op corresponding to `mode`. After this,
    /// `selection_active()` is true (unless `mode` was Subtract and no
    /// selection existed, in which case the op was a no-op).
    pub fn apply_selection_shape(
        &mut self,
        shape: &SelectionShape,
        mode: SelectionMode,
    ) -> Result<(), RendererError> {
        let size = self.renderer.canvas_size();
        let pixels = crate::selection::rasterise(shape, size.width, size.height);
        let blend = match mode {
            SelectionMode::Replace => SelectionBlendMode::Replace,
            SelectionMode::Add => SelectionBlendMode::Add,
            SelectionMode::Subtract => SelectionBlendMode::Subtract,
            SelectionMode::Intersect => SelectionBlendMode::Intersect,
        };
        self.renderer.apply_selection_shape(&pixels, blend)?;
        self.bump_version();
        Ok(())
    }

    /// Whether the mask currently affects compositing.
    #[must_use]
    pub const fn selection_active(&self) -> bool {
        self.renderer.selection_active()
    }

    /// Run the GPU edge/downsample pass and return the small R8 buffer.
    /// The caller (the UI) feeds this to `crate::selection::marching_squares`
    /// to get contour polylines, then scales each coordinate by
    /// `canvas_w / edges_w` etc. to put them back in canvas-pixel space.
    pub fn read_selection_edges(&mut self) -> Result<EdgesBuffer, RendererError> {
        self.renderer.compute_selection_edges()
    }

    /// Read the full-resolution selection mask as a row-major R8 buffer.
    /// Used by the pixel-perfect ants tracer.
    pub fn read_selection_mask(&mut self) -> Result<Vec<u8>, RendererError> {
        self.renderer.read_selection_mask()
    }

    /// Split the layer at `idx` into two BGRA8 buffers using the selection
    /// mask: the *masked* pixels (layer x mask, used for transform / cut /
    /// copy) and the *remaining* pixels (layer x (1-mask), which becomes
    /// the layer's new content). Writes the remaining pixels back to the
    /// GPU layer and clears the selection (the marquee no longer makes
    /// sense once the pixels have been lifted).
    ///
    /// Returns `(masked_pixels, canvas_w, canvas_h)`. Returns `Ok(None)`
    /// if no selection is currently active. The caller can compute tight
    /// non-empty bounds from `masked_pixels` itself.
    pub fn extract_selection_pixels(
        &mut self,
        idx: usize,
    ) -> Result<Option<(Vec<u8>, u32, u32)>, RendererError> {
        if !self.renderer.selection_active() {
            return Ok(None);
        }
        let size = self.renderer.canvas_size();
        let layer = self.renderer.read_layer(idx)?;
        let mask = self.renderer.read_selection_mask()?;
        let (masked, remaining) = split_layer_by_mask(&layer, &mask);
        self.renderer.write_layer(idx, &remaining)?;
        self.normalize_adjustment_slot(idx)?;
        self.deselect();
        self.recomposite_canvas()?;
        Ok(Some((masked, size.width, size.height)))
    }

    /// Apply the selection mask to a copy of the layer pixels without
    /// modifying the layer or the mask. Used by `Copy`. Returns the
    /// canvas-sized BGRA8 buffer or `None` if no selection is active.
    pub fn read_selection_pixels(
        &mut self,
        idx: usize,
    ) -> Result<Option<(Vec<u8>, u32, u32)>, RendererError> {
        if !self.renderer.selection_active() {
            return Ok(None);
        }
        let size = self.renderer.canvas_size();
        let layer = self.renderer.read_layer(idx)?;
        let mask = self.renderer.read_selection_mask()?;
        let (masked, _remaining) = split_layer_by_mask(&layer, &mask);
        Ok(Some((masked, size.width, size.height)))
    }

    /// Clear only the selected pixels on the layer at `idx` (layer x
    /// (1 - mask)) and deselect. Used by `Cut` after the masked pixels
    /// have already been copied to the clipboard. No-op without a
    /// selection.
    pub fn clear_selection_from_layer(&mut self, idx: usize) -> Result<(), RendererError> {
        if !self.renderer.selection_active() {
            return Ok(());
        }
        let layer = self.renderer.read_layer(idx)?;
        let mask = self.renderer.read_selection_mask()?;
        let (_masked, remaining) = split_layer_by_mask(&layer, &mask);
        self.renderer.write_layer(idx, &remaining)?;
        self.normalize_adjustment_slot(idx)?;
        self.deselect();
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Erase the selected pixels on the layer at `idx` (layer x (1 - mask))
    /// while keeping the selection active. Used by the Delete key so the
    /// marquee stays put and the user can keep editing inside it. No-op
    /// without a selection.
    pub fn erase_selection_in_layer(&mut self, idx: usize) -> Result<(), RendererError> {
        if !self.renderer.selection_active() {
            return Ok(());
        }
        let layer = self.renderer.read_layer(idx)?;
        let mask = self.renderer.read_selection_mask()?;
        let (_masked, remaining) = split_layer_by_mask(&layer, &mask);
        self.renderer.write_layer(idx, &remaining)?;
        self.normalize_adjustment_slot(idx)?;
        self.recomposite_canvas()?;
        Ok(())
    }

    /// Replace the selection mask with one derived from the alpha
    /// channel of the layer at `idx`: the selection strength equals the
    /// layer's alpha, so anti-aliased edges stay soft. Used when clicking
    /// a layer's thumbnail in the panel.
    pub fn select_from_layer_alpha(&mut self, idx: usize) -> Result<(), RendererError> {
        self.select_from_layers_alpha(&[idx])
    }

    /// Replace the selection mask with the union of several layers' alpha
    /// channels (per-pixel max), so anti-aliased edges stay soft. Used when
    /// clicking a folder's icon to select everything inside it.
    pub fn select_from_layers_alpha(&mut self, indices: &[usize]) -> Result<(), RendererError> {
        let mut shape: Vec<u8> = Vec::new();
        for &idx in indices {
            let layer = self.renderer.read_layer(idx)?;
            let n = layer.len() / 4;
            if shape.is_empty() {
                shape = vec![0u8; n];
            }
            for i in 0..n.min(shape.len()) {
                // BGRA8 -> alpha is byte 3; take the strongest coverage so a
                // pixel painted on any layer in the set is selected.
                shape[i] = shape[i].max(layer[i * 4 + 3]);
            }
        }
        if shape.is_empty() {
            return Ok(());
        }
        self.renderer
            .apply_selection_shape(&shape, SelectionBlendMode::Replace)?;
        self.bump_version();
        Ok(())
    }
}

/// Split BGRA8 `layer` by R8 `mask`: returns `(masked, remaining)` where
/// `masked = layer * (mask/255)` and `remaining = layer * (1 - mask/255)`.
/// All four channels (premultiplied alpha) are scaled, so the result is
/// still premultiplied BGRA8.
fn split_layer_by_mask(layer: &[u8], mask: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let n = mask.len().min(layer.len() / 4);
    let mut masked = vec![0u8; n * 4];
    let mut remaining = vec![0u8; n * 4];
    for i in 0..n {
        let m = u32::from(mask[i]);
        let inv = 255 - m;
        for c in 0..4 {
            let v = u32::from(layer[i * 4 + c]);
            // Round to nearest, divide by 255.
            masked[i * 4 + c] = ((v * m + 127) / 255) as u8;
            remaining[i * 4 + c] = ((v * inv + 127) / 255) as u8;
        }
    }
    (masked, remaining)
}

// CPU pixel helpers (crop_bgra8, transform_bgra8, sample_*) live in
// oxiedraw_utils::pixels - see the use statement at the top of this file.

impl std::fmt::Debug for Canvas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Canvas")
            .field("size", &self.size())
            .field("layers", &self.layers.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use oxiedraw_utils::geometry::Point;

    use crate::brush_engine::{BrushEngine, InputSample};

    use super::*;

    fn sample(x: f32, y: f32, t: u64) -> InputSample {
        InputSample {
            position: Point::new(x, y),
            pressure: 1.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation: 0.0,
            time_ms: t,
        }
    }

    /// A `BrushEngine` at `size` with crisp-edged presets.
    ///
    /// Default Round is an airbrush (`hardness: 0.02`), so its strokes peak
    /// around 0.93 between dab centres and never saturate. The tests below are
    /// about compositing rather than falloff, so they pin the edge instead of
    /// tracking whatever the default preset is tuned to.
    fn crisp_brush(size: f32) -> BrushEngine {
        let brush = BrushEngine::new();
        for preset in brush.brushes.borrow_mut().iter_mut() {
            preset.hardness = 1.0;
        }
        brush.size.set(size);
        brush.opacity.set(1.0);
        brush
    }

    // Backs `erase_selection_in_layer` / `clear_selection_from_layer`: a fully
    // masked pixel moves entirely into `masked`, an unmasked one stays in
    // `remaining`, and a half-mask splits the value across both.
    #[test]
    fn split_layer_by_mask_partitions_pixels() {
        // Three pixels, every channel = 200.
        let layer = vec![200u8; 3 * 4];
        let mask = [255u8, 0, 128];

        let (masked, remaining) = split_layer_by_mask(&layer, &mask);

        // mask = 255: all value to masked, none remains.
        assert_eq!(&masked[0..4], &[200, 200, 200, 200]);
        assert_eq!(&remaining[0..4], &[0, 0, 0, 0]);
        // mask = 0: nothing masked, all remains.
        assert_eq!(&masked[4..8], &[0, 0, 0, 0]);
        assert_eq!(&remaining[4..8], &[200, 200, 200, 200]);
        // mask = 128: split. masked + remaining should reconstruct the original
        // (within rounding) and each part is roughly half.
        for c in 0..4 {
            let m = i32::from(masked[8 + c]);
            let r = i32::from(remaining[8 + c]);
            assert!((m - 100).abs() <= 1, "masked half off: {m}");
            assert!((r - 100).abs() <= 1, "remaining half off: {r}");
            assert!((m + r - 200).abs() <= 1, "split must sum to original");
        }
    }

    /// Drive a complete brush stroke through the brush engine and
    /// `Canvas`, then assert the painted pixels look right. Exercises
    /// the full path: `BrushEngine` -> `PaintTarget` adapter ->
    /// `stamp_mask` -> `composite_stroke_into_layer` -> recomposite ->
    /// readback.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn end_to_end_stroke() {
        let size = Size::new(128, 64);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        let brush = crisp_brush(8.0);

        let red = Color::new(255, 0, 0);

        canvas.begin_stroke(red, 1.0, false).expect("begin_stroke");

        let mut iter = (0_u32..10).map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let x = (i as f32).mul_add(10.0, 16.0);
            sample(x, 32.0, u64::from(i) * 10)
        });
        let first = iter.next().expect("non-empty");
        canvas
            .stamp(|t| brush.begin_stroke(first, red, t))
            .expect("brush.begin");
        for s in iter {
            canvas
                .stamp(|t| brush.push_sample(s, t))
                .expect("brush.push");
        }
        canvas.stamp(|t| brush.end_stroke(t)).expect("brush.end");

        canvas.commit_stroke().expect("commit");

        let bytes = canvas.read_pixels().expect("readback");

        // Pixel under the stroke center should be solid red. Canvas
        // format is BGRA so R is at center+2.
        let center = (32 * 128 + 64) * 4;
        assert!(bytes[center] <= 0x10, "stroke B={:02x}", bytes[center]);
        assert!(
            bytes[center + 1] <= 0x10,
            "stroke G={:02x}",
            bytes[center + 1]
        );
        assert!(
            bytes[center + 2] >= 0xF0,
            "stroke R={:02x}",
            bytes[center + 2]
        );
        assert!(
            bytes[center + 3] >= 0xF0,
            "stroke A={:02x}",
            bytes[center + 3]
        );

        // Far-from-stroke corner stays transparent.
        assert_eq!(&bytes[..4], &[0x00, 0x00, 0x00, 0x00]);
    }

    /// A resize recreates the renderer; drawing must keep working afterward.
    /// Guards the shared-device path: instance/device are created once and
    /// reused, so the post-resize renderer paints correctly and the original
    /// content survives the crop. Regression test for stylus-draw lag that
    /// only appeared after an in-session canvas resize.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn draw_after_resize_paints() {
        let mut canvas = Canvas::headless(Size::new(128, 64)).expect("canvas init");
        let brush = crisp_brush(8.0);
        let red = Color::new(255, 0, 0);

        // Helper: paint a short horizontal red stroke centred on `cx`.
        let stroke = |canvas: &mut Canvas, cx: f32| {
            canvas.begin_stroke(red, 1.0, false).expect("begin_stroke");
            let mut iter = (0_u32..5).map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let x = (i as f32).mul_add(6.0, cx - 12.0);
                sample(x, 32.0, u64::from(i) * 10)
            });
            let first = iter.next().expect("non-empty");
            canvas
                .stamp(|t| brush.begin_stroke(first, red, t))
                .expect("brush.begin");
            for s in iter {
                canvas.stamp(|t| brush.push_sample(s, t)).expect("brush.push");
            }
            canvas.stamp(|t| brush.end_stroke(t)).expect("brush.end");
            canvas.commit_stroke().expect("commit");
        };

        // Count opaque-red pixels in a region (the default brush is soft /
        // speed-dynamic, so assert on coverage rather than an exact pixel).
        let red_count = |bytes: &[u8], stride: usize, x0: usize, x1: usize, y0: usize, y1: usize| {
            let mut n = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * stride + x) * 4;
                    if bytes[i + 2] >= 0xF0 && bytes[i + 3] >= 0xF0 {
                        n += 1;
                    }
                }
            }
            n
        };

        // Paint on the original canvas, then expand the width to 192.
        stroke(&mut canvas, 32.0);
        let new_size = canvas
            .apply_crop(CropRect::new(0.0, 0.0, 192.0, 64.0))
            .expect("apply_crop");
        assert_eq!(new_size, Size::new(192, 64), "canvas should widen to 192");

        // Draw again, in the newly-added region, on the recreated renderer.
        stroke(&mut canvas, 160.0);

        let bytes = canvas.read_pixels().expect("readback");
        // The post-resize stroke painted on the recreated (shared-device) renderer.
        assert!(
            red_count(&bytes, 192, 144, 176, 24, 40) > 20,
            "post-resize stroke did not paint",
        );
        // The first stroke survived the crop (content preserved at offset 0).
        assert!(
            red_count(&bytes, 192, 8, 56, 24, 40) > 20,
            "pre-resize content lost after crop",
        );
        // Untouched area in the expanded region stays transparent.
        assert_eq!(
            red_count(&bytes, 192, 176, 192, 0, 16),
            0,
            "expanded area should be transparent",
        );
    }

    /// The incremental (dab-region-clipped) preview must produce the same image
    /// as a full rebuild: dabs from earlier frames are retained outside the
    /// current dab region, and the current region is recomposited correctly.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn incremental_preview_matches_full_rebuild() {
        let size = Size::new(64, 64);
        let mut canvas = Canvas::headless(size).expect("init");
        let red: Vec<u8> = (0..size.width * size.height)
            .flat_map(|_| [0u8, 0, 255, 255])
            .collect();
        let base = canvas.add_layer_with_pixels("base", &red).expect("base");
        canvas.layers().set_active(Some(base));

        let brush = BrushEngine::new();
        brush.size.set(6.0);
        brush.opacity.set(1.0);
        let white = Color::new(255, 255, 255);
        canvas.begin_stroke(white, 1.0, false).expect("begin");

        // Frame 1 (full): a dab near the top-left.
        canvas
            .stamp(|t| brush.begin_stroke(sample(12.0, 12.0, 0), white, t))
            .expect("b");
        canvas
            .stamp(|t| brush.push_sample(sample(16.0, 12.0, 10), t))
            .expect("p");
        let _ = canvas.read_incremental_preview().expect("inc1");

        // Frame 2 (incremental): extend the stroke toward the bottom-right.
        canvas
            .stamp(|t| brush.push_sample(sample(48.0, 50.0, 20), t))
            .expect("p");
        canvas
            .stamp(|t| brush.push_sample(sample(52.0, 52.0, 30), t))
            .expect("p");
        let incremental = canvas.read_incremental_preview().expect("inc2");

        // Full rebuild of the same stroke state.
        canvas.force_full_preview();
        let full = canvas.read_incremental_preview().expect("full");

        assert_eq!(incremental.len(), full.len());
        let diff = incremental
            .iter()
            .zip(full.iter())
            .filter(|(a, b)| (i32::from(**a) - i32::from(**b)).abs() > 2)
            .count();
        assert_eq!(diff, 0, "incremental diverged from full rebuild in {diff} bytes");
    }

    /// An eraser stroke on the top layer removes its coverage and reveals
    /// the layer below, without touching the layer below. Drives the same
    /// brush path with `erase = true`.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn end_to_end_eraser_reveals_lower_layer() {
        let size = Size::new(64, 64);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        // BGRA8, opaque. Bottom = green, top = red.
        let n = (size.width * size.height) as usize;
        let green: Vec<u8> = [0u8, 255, 0, 255].iter().copied().cycle().take(n * 4).collect();
        let red: Vec<u8> = [0u8, 0, 255, 255].iter().copied().cycle().take(n * 4).collect();
        canvas.add_layer_with_pixels("bottom", &green).expect("bottom");
        let top = canvas.add_layer_with_pixels("top", &red).expect("top");
        assert_eq!(canvas.layers().active(), Some(top), "top layer active");

        let brush = crisp_brush(12.0);
        let stroke_color = Color::new(0, 0, 0);

        canvas.begin_stroke(stroke_color, 1.0, true).expect("begin erase");
        canvas
            .stamp(|t| brush.begin_stroke(sample(32.0, 32.0, 0), stroke_color, t))
            .expect("brush.begin");
        canvas.stamp(|t| brush.end_stroke(t)).expect("brush.end");
        canvas.commit_stroke().expect("commit");

        let bytes = canvas.read_pixels().expect("readback");
        let at = |x: usize, y: usize| {
            let i = (y * 64 + x) * 4;
            (bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3])
        };

        // Under the eraser the top red is gone, so the green below shows
        // through (opaque). BGRA: green is (0, 255, 0, 255).
        let (b, g, r, a) = at(32, 32);
        assert!(g >= 0xF0, "center should reveal green G={g:02x}");
        assert!(r <= 0x10, "center red removed R={r:02x}");
        assert!(b <= 0x10, "center B={b:02x}");
        assert_eq!(a, 255, "center stays opaque (green below)");

        // The top layer itself is transparent where erased.
        let top_px = canvas.read_layer(top).expect("read top");
        assert_eq!(top_px[(32 * 64 + 32) * 4 + 3], 0, "top erased to transparent");

        // Far corner is untouched: top red still on top.
        let (_, _, r_corner, a_corner) = at(2, 2);
        assert!(r_corner >= 0xF0, "corner keeps red R={r_corner:02x}");
        assert_eq!(a_corner, 255, "corner opaque");
    }

    /// Lift-and-apply with an identity transform should produce the
    /// original pixels back on the layer. Proves Apply OVER-blends the
    /// transformed pixels onto the unmasked region instead of replacing
    /// it.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn transform_apply_preserves_unmasked_pixels() {
        use crate::selection::{RectShape, SelectionShape};
        use crate::tools::SelectionMode;
        use oxiedraw_utils::geometry::TransformRect;

        let size = Size::new(32, 32);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        // Paint solid red across the whole layer.
        let brush = BrushEngine::new();
        brush.size.set(80.0);
        brush.opacity.set(1.0);
        let red = Color::new(255, 0, 0);
        canvas.begin_stroke(red, 1.0, false).expect("begin");
        canvas
            .stamp(|t| brush.begin_stroke(sample(16.0, 16.0, 0), red, t))
            .expect("stamp");
        canvas.stamp(|t| brush.end_stroke(t)).expect("end");
        canvas.commit_stroke().expect("commit");

        // Select the left half.
        let shape = SelectionShape::Rect(RectShape {
            x: 0.0,
            y: 0.0,
            w: 16.0,
            h: 32.0,
        });
        canvas
            .apply_selection_shape(&shape, SelectionMode::Replace)
            .expect("apply selection");

        // Lift: layer keeps right half, lifted = left half.
        let (lifted, lw, lh) = canvas
            .extract_selection_pixels(0)
            .expect("extract")
            .expect("had selection");

        // Identity transform: original_rect == current_rect over the
        // tight bounds of the lifted pixels.
        let orig_rect = TransformRect::new(8.0, 16.0, 16.0, 32.0, 0.0);
        let current_rect = orig_rect;
        canvas
            .apply_layer_transform_gpu(0, &lifted, lw, lh, orig_rect, current_rect)
            .expect("apply gpu");

        // Layer should now match the pre-lift state: both halves red.
        let bytes = canvas.read_layer(0).expect("read");
        let left_i = (16 * 32 + 8) * 4;
        assert!(bytes[left_i + 2] >= 0xF0, "left half R={:02x}", bytes[left_i + 2]);
        let right_i = (16 * 32 + 24) * 4;
        assert!(
            bytes[right_i + 2] >= 0xF0,
            "right half R={:02x} (must be preserved by OVER blend)",
            bytes[right_i + 2]
        );
    }

    /// Translating a lifted selection by an integer pixel offset must
    /// (a) place the moved pixels exactly at the new position with no
    /// fractional bleeding, and (b) leave the rest of the layer alone
    /// (i.e. the unmasked region must survive Apply).
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn translate_selection_lands_on_pixel_grid() {
        use crate::selection::{RectShape, SelectionShape};
        use crate::tools::SelectionMode;
        use oxiedraw_utils::geometry::TransformRect;

        let size = Size::new(32, 32);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        // Paint a 4x4 red square at (4..8, 4..8) by stamping a dab.
        let brush = BrushEngine::new();
        brush.size.set(4.0);
        brush.opacity.set(1.0);
        let red = Color::new(255, 0, 0);
        canvas.begin_stroke(red, 1.0, false).expect("begin");
        canvas
            .stamp(|t| brush.begin_stroke(sample(6.0, 6.0, 0), red, t))
            .expect("stamp");
        canvas.stamp(|t| brush.end_stroke(t)).expect("end");
        canvas.commit_stroke().expect("commit");

        // Paint a separate green square at (20..24, 20..24) - unmasked
        // by the selection below, must survive Apply.
        let green = Color::new(0, 255, 0);
        canvas.begin_stroke(green, 1.0, false).expect("begin");
        canvas
            .stamp(|t| brush.begin_stroke(sample(22.0, 22.0, 0), green, t))
            .expect("stamp");
        canvas.stamp(|t| brush.end_stroke(t)).expect("end");
        canvas.commit_stroke().expect("commit");

        // Select a region tightly around the red square.
        canvas
            .apply_selection_shape(
                &SelectionShape::Rect(RectShape {
                    x: 4.0,
                    y: 4.0,
                    w: 4.0,
                    h: 4.0,
                }),
                SelectionMode::Replace,
            )
            .expect("select");

        let (lifted, lw, lh) = canvas
            .extract_selection_pixels(0)
            .expect("extract")
            .expect("had selection");

        // Translate +8 px right, +8 px down (integer offset).
        let orig_rect = TransformRect::new(6.0, 6.0, 4.0, 4.0, 0.0);
        let current_rect = TransformRect::new(14.0, 14.0, 4.0, 4.0, 0.0);
        canvas
            .apply_layer_transform_gpu(0, &lifted, lw, lh, orig_rect, current_rect)
            .expect("apply");

        let bytes = canvas.read_layer(0).expect("read");
        let px_at = |x: u32, y: u32| {
            let i = ((y * 32 + x) * 4) as usize;
            (bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3])
        };

        // Red should now sit at the new centre (around (14, 14)). Sample
        // the very centre.
        let (b, g, r, a) = px_at(14, 14);
        assert!(r > 0x80, "translated red R={r:02x} at (14,14)");
        assert!(a > 0x80);
        let _ = (b, g);

        // Green square must be untouched.
        let (_, g, _, a) = px_at(22, 22);
        assert!(g > 0x80, "green survived G={g:02x}");
        assert!(a > 0x80);

        // Old position (4..8) must be empty (it was lifted).
        let (_, _, _, a) = px_at(6, 6);
        assert!(a < 0x10, "old red position should be transparent A={a:02x}");
    }

    /// extract_selection_pixels lifts the masked pixels off the layer
    /// and leaves the unmasked region behind.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn extract_selection_lifts_masked_pixels() {
        use crate::selection::{RectShape, SelectionShape};
        use crate::tools::SelectionMode;

        let size = Size::new(32, 32);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        // Paint a solid red square covering the canvas.
        let brush = BrushEngine::new();
        brush.size.set(80.0);
        brush.opacity.set(1.0);
        let red = Color::new(255, 0, 0);
        canvas.begin_stroke(red, 1.0, false).expect("begin");
        canvas
            .stamp(|t| brush.begin_stroke(sample(16.0, 16.0, 0), red, t))
            .expect("stamp");
        canvas.stamp(|t| brush.end_stroke(t)).expect("end");
        canvas.commit_stroke().expect("commit");

        // Selection: left half.
        let shape = SelectionShape::Rect(RectShape {
            x: 0.0,
            y: 0.0,
            w: 16.0,
            h: 32.0,
        });
        canvas
            .apply_selection_shape(&shape, SelectionMode::Replace)
            .expect("apply selection");

        let (lifted, w, h) = canvas
            .extract_selection_pixels(0)
            .expect("extract ok")
            .expect("had selection");
        assert_eq!(w, 32);
        assert_eq!(h, 32);

        // Lifted buffer: left half should be red, right half zero.
        let left_i = (16 * 32 + 8) * 4;
        assert!(lifted[left_i + 2] >= 0xF0, "lifted left R={:02x}", lifted[left_i + 2]);
        let right_i = (16 * 32 + 24) * 4;
        assert_eq!(
            &lifted[right_i..right_i + 4],
            &[0x00, 0x00, 0x00, 0x00],
            "lifted right should be empty"
        );

        // Layer remaining: left half should be empty, right half still red.
        let layer = canvas.read_layer(0).expect("read layer");
        let l = (16 * 32 + 8) * 4;
        assert_eq!(
            &layer[l..l + 4],
            &[0x00, 0x00, 0x00, 0x00],
            "layer left should be cleared"
        );
        let r = (16 * 32 + 24) * 4;
        assert!(layer[r + 2] >= 0xF0, "layer right R={:02x}", layer[r + 2]);

        // Selection is cleared after the lift.
        assert!(!canvas.selection_active(), "lift should deselect");
    }

    /// select_from_layer_alpha turns an arbitrary layer's non-zero alpha
    /// region into a selection mask.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn select_from_layer_alpha_builds_mask() {
        let size = Size::new(32, 32);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        // Paint a red blob centred at (8, 16).
        let brush = crisp_brush(10.0);
        let red = Color::new(255, 0, 0);
        canvas.begin_stroke(red, 1.0, false).expect("begin");
        canvas
            .stamp(|t| brush.begin_stroke(sample(8.0, 16.0, 0), red, t))
            .expect("stamp");
        canvas.stamp(|t| brush.end_stroke(t)).expect("end");
        canvas.commit_stroke().expect("commit");

        canvas.select_from_layer_alpha(0).expect("select");
        assert!(canvas.selection_active());

        let mask = canvas.read_selection_mask().expect("read mask");
        // Centre of the blob should be selected.
        assert_eq!(mask[16 * 32 + 8], 0xFF);
        // Far corner should be unselected.
        assert_eq!(mask[31 * 32 + 31], 0x00);
    }

    /// Brush stroke composited through a selection mask gets clipped:
    /// pixels inside the mask receive the stroke colour; pixels outside
    /// stay transparent.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn brush_stroke_clipped_to_selection() {
        use crate::selection::{RectShape, SelectionShape};
        use crate::tools::SelectionMode;

        let size = Size::new(64, 64);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        // Selection: left half of canvas.
        let shape = SelectionShape::Rect(RectShape {
            x: 0.0,
            y: 0.0,
            w: 32.0,
            h: 64.0,
        });
        canvas
            .apply_selection_shape(&shape, SelectionMode::Replace)
            .expect("apply selection");
        assert!(canvas.selection_active());

        // Paint a stroke spanning the whole canvas horizontally.
        let brush = BrushEngine::new();
        brush.size.set(20.0);
        brush.opacity.set(1.0);
        let red = Color::new(255, 0, 0);

        canvas.begin_stroke(red, 1.0, false).expect("begin");
        canvas
            .stamp(|t| brush.begin_stroke(sample(16.0, 32.0, 0), red, t))
            .expect("stamp left");
        canvas
            .stamp(|t| brush.push_sample(sample(48.0, 32.0, 1), t))
            .expect("stamp right");
        canvas.stamp(|t| brush.end_stroke(t)).expect("end");
        canvas.commit_stroke().expect("commit");

        let bytes = canvas.read_pixels().expect("readback");

        // Inside selection (x=16, y=32) - should be red.
        let inside = (32 * 64 + 16) * 4;
        assert!(bytes[inside + 2] >= 0xF0, "inside R={:02x}", bytes[inside + 2]);

        // Outside selection (x=48, y=32) - should be untouched (transparent).
        let outside = (32 * 64 + 48) * 4;
        assert_eq!(
            &bytes[outside..outside + 4],
            &[0x00, 0x00, 0x00, 0x00],
            "outside should be untouched"
        );
    }

    /// During a stroke, `read_pixels` should return the preview
    /// (canvas + tinted stroke). Discarding then reading should show
    /// the canvas untouched.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn preview_during_stroke() {
        let size = Size::new(64, 64);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        let red = Color::new(255, 0, 0);
        canvas.begin_stroke(red, 1.0, false).expect("begin");
        let brush = BrushEngine::new();
        brush.size.set(40.0);
        brush.opacity.set(1.0);
        canvas
            .stamp(|t| brush.begin_stroke(sample(32.0, 32.0, 0), red, t))
            .expect("stamp");

        let preview = canvas.read_pixels().expect("preview readback");
        let center = (32 * 64 + 32) * 4;
        assert!(
            preview[center + 2] >= 0xF0,
            "preview R={:02x}",
            preview[center + 2]
        );
        assert!(
            preview[center + 3] >= 0xF0,
            "preview A={:02x}",
            preview[center + 3]
        );

        canvas.discard_stroke().expect("discard");
        let after = canvas.read_pixels().expect("after readback");
        assert_eq!(
            &after[center..center + 4],
            &[0x00, 0x00, 0x00, 0x00],
            "canvas should be untouched"
        );
    }

    /// Multi-layer test: bottom layer red, top layer green at a
    /// different position. Composite should show both colors at
    /// their respective positions.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn two_layers_composite() {
        let size = Size::new(64, 64);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        let brush = BrushEngine::new();
        brush.size.set(20.0);
        brush.opacity.set(1.0);

        // Layer 0 (background): paint red at (16, 32).
        let red = Color::new(255, 0, 0);
        canvas.begin_stroke(red, 1.0, false).expect("begin red");
        canvas
            .stamp(|t| brush.begin_stroke(sample(16.0, 32.0, 0), red, t))
            .expect("stamp");
        canvas.stamp(|t| brush.end_stroke(t)).expect("end");
        canvas.commit_stroke().expect("commit red");

        // Add a new layer, paint green at (48, 32).
        canvas.add_layer("Top").expect("add layer");
        let green = Color::new(0, 255, 0);
        canvas.begin_stroke(green, 1.0, false).expect("begin green");
        canvas
            .stamp(|t| brush.begin_stroke(sample(48.0, 32.0, 0), green, t))
            .expect("stamp");
        canvas.stamp(|t| brush.end_stroke(t)).expect("end");
        canvas.commit_stroke().expect("commit green");

        let bytes = canvas.read_pixels().expect("read");

        // Pixel (16, 32) should be red.
        let red_i = (32 * 64 + 16) * 4;
        assert!(bytes[red_i + 2] >= 0xF0, "red R={:02x}", bytes[red_i + 2]);
        assert!(bytes[red_i + 1] <= 0x10, "red G={:02x}", bytes[red_i + 1]);

        // Pixel (48, 32) should be green.
        let green_i = (32 * 64 + 48) * 4;
        assert!(
            bytes[green_i + 1] >= 0xF0,
            "green G={:02x}",
            bytes[green_i + 1]
        );
        assert!(
            bytes[green_i + 2] <= 0x10,
            "green R={:02x}",
            bytes[green_i + 2]
        );
    }

    /// In-flight preview with the stroke on a *lower* layer and an opaque
    /// layer above it. Exercises the cached below-stack composite, the
    /// above-layer re-composite, and (by reading twice) cache reuse.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn region_upload_changes_only_region_and_bumps_version() {
        let size = Size::new(16, 8);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        // A layer filled solid opaque red (BGRA premultiplied == straight here).
        let red = [0u8, 0, 255, 255].repeat((size.width * size.height) as usize);
        let idx = canvas.add_layer_with_pixels("L", &red).expect("add layer");
        let v0 = canvas.layer_content_version(idx);

        // Upload a 4x4 opaque-blue region at (2, 1).
        let blue = [255u8, 0, 0, 255].repeat(4 * 4);
        canvas
            .restore_layer_region(idx, 2, 1, 4, 4, &blue)
            .expect("region upload");

        let v1 = canvas.layer_content_version(idx);
        assert!(v1 > v0, "region upload must bump the layer version");

        let out = canvas.read_layer(idx).expect("read layer");
        let px = |x: usize, y: usize| {
            let i = (y * size.width as usize + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };
        // Inside the region: blue. Outside: untouched red.
        assert_eq!(px(3, 2), [255, 0, 0, 255], "inside region should be blue");
        assert_eq!(px(0, 0), [0, 0, 255, 255], "top-left should stay red");
        assert_eq!(px(10, 6), [0, 0, 255, 255], "bottom-right should stay red");
        // Region edges (x in 2..6, y in 1..5).
        assert_eq!(px(2, 1), [255, 0, 0, 255], "region corner blue");
        assert_eq!(px(6, 1), [0, 0, 255, 255], "just past region stays red");
    }

    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn preview_composites_above_layer_over_lower_stroke() {
        use crate::brush_engine::Dab;

        let size = Size::new(64, 64);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        let brush = crisp_brush(20.0);

        // Top layer (1): opaque green disc on the left at (16, 32).
        canvas.add_layer("Top").expect("add layer");
        let green = Color::new(0, 255, 0);
        canvas.begin_stroke(green, 1.0, false).expect("begin green");
        canvas
            .stamp(|t| brush.begin_stroke(sample(16.0, 32.0, 0), green, t))
            .expect("stamp green");
        canvas.stamp(|t| brush.end_stroke(t)).expect("end green");
        canvas.commit_stroke().expect("commit green");

        // Paint a red stroke on the BELOW layer (0): one dab under the
        // green (16, 32) and one in the clear area (48, 32).
        canvas.layers().set_active(Some(0));
        let red = Color::new(255, 0, 0);
        canvas.begin_stroke(red, 1.0, false).expect("begin red");
        let dabs = [
            Dab::round(Point::new(16.0, 32.0), 8.0, red),
            Dab::round(Point::new(48.0, 32.0), 8.0, red),
        ];
        canvas.stamp(|t| t.paint_dabs(&dabs)).expect("stamp red");

        // Read the in-flight preview twice: the first builds the below
        // cache, the second must reuse it and match byte-for-byte.
        let first = canvas.read_pixels().expect("read 1");
        let second = canvas.read_pixels().expect("read 2");
        assert_eq!(first, second, "cached preview must equal a fresh build");

        // Under the green (16, 32): opaque green on top wins.
        let g_i = (32 * 64 + 16) * 4;
        assert!(first[g_i + 1] >= 0xF0, "green over stroke G={:02x}", first[g_i + 1]);
        assert!(first[g_i + 2] <= 0x10, "green over stroke R={:02x}", first[g_i + 2]);

        // Clear area (48, 32): the in-flight red stroke shows through.
        let r_i = (32 * 64 + 48) * 4;
        assert!(first[r_i + 2] >= 0xF0, "red stroke R={:02x}", first[r_i + 2]);
        assert!(first[r_i + 1] <= 0x10, "red stroke G={:02x}", first[r_i + 1]);
    }

    /// An in-flight eraser stroke's preview must reveal the layer below
    /// the target (where erased) while leaving the rest of the target on
    /// top. Exercises the `record_layered_preview` erase branch (scratch
    /// build + below-cache exclude) without committing.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn preview_eraser_reveals_lower_layer() {
        use crate::brush_engine::Dab;

        let size = Size::new(64, 64);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        let n = (size.width * size.height) as usize;
        let green: Vec<u8> = [0u8, 255, 0, 255].iter().copied().cycle().take(n * 4).collect();
        let red: Vec<u8> = [0u8, 0, 255, 255].iter().copied().cycle().take(n * 4).collect();
        canvas.add_layer_with_pixels("bottom", &green).expect("bottom");
        let top = canvas.add_layer_with_pixels("top", &red).expect("top");
        assert_eq!(canvas.layers().active(), Some(top), "top active");

        let stroke_color = Color::new(0, 0, 0);
        canvas.begin_stroke(stroke_color, 1.0, true).expect("begin erase");
        let dabs = [Dab::round(Point::new(32.0, 32.0), 10.0, stroke_color)];
        canvas.stamp(|t| t.paint_dabs(&dabs)).expect("stamp erase");

        // Two reads: the first builds the below cache (excluding the target),
        // the second must reuse it and match byte-for-byte.
        let first = canvas.read_pixels().expect("read 1");
        let second = canvas.read_pixels().expect("read 2");
        assert_eq!(first, second, "cached erase preview must equal a fresh build");

        // Under the eraser (32, 32): top red removed, green below revealed.
        let c = (32 * 64 + 32) * 4;
        assert!(first[c + 1] >= 0xF0, "center reveal green G={:02x}", first[c + 1]);
        assert!(first[c + 2] <= 0x10, "center red removed R={:02x}", first[c + 2]);
        assert_eq!(first[c + 3], 255, "center opaque (green below)");

        // Far corner: top red still shown.
        let k = (2 * 64 + 2) * 4;
        assert!(first[k + 2] >= 0xF0, "corner keeps red R={:02x}", first[k + 2]);
        assert!(first[k + 1] <= 0x10, "corner G={:02x}", first[k + 1]);

        // Discarding must restore the canvas: top red everywhere again.
        canvas.discard_stroke().expect("discard");
        let after = canvas.read_pixels().expect("read after discard");
        let c2 = (32 * 64 + 32) * 4;
        assert!(after[c2 + 2] >= 0xF0, "after discard red restored R={:02x}", after[c2 + 2]);
    }

    /// The fused stamp+present path must actually deposit the dab into the
    /// stroke buffer (visible in the preview readback) and accumulate the
    /// dirty rect, just like the separate stamp + present calls.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn stamp_and_present_lands_in_stroke() {
        use crate::brush_engine::Dab;

        let size = Size::new(64, 64);
        let mut canvas = Canvas::headless(size).expect("canvas init");
        let red = Color::new(255, 0, 0);

        canvas.begin_stroke(red, 1.0, false).expect("begin");
        let dabs = [Dab::round(Point::new(32.0, 32.0), 12.0, red)];
        canvas
            .stamp_and_present(|t| t.paint_dabs(&dabs))
            .expect("stamp_and_present");

        // The in-flight preview must show the red stroke at the dab centre.
        let px = canvas.read_pixels().expect("read");
        let i = (32 * 64 + 32) * 4;
        assert!(px[i + 2] >= 0xF0, "R={:02x}", px[i + 2]);
        assert!(px[i + 1] <= 0x10, "G={:02x}", px[i + 1]);

        // History dirty-rect tracking must work through the fused path.
        assert!(
            canvas.stroke_dirty_bounds().is_some(),
            "fused stamp must accumulate the dirty rect"
        );
    }

    /// The dab-quad dirty rect must cover every changed pixel, and a patch
    /// built from just that region must equal the canonical full-canvas
    /// diff. This is the correctness guarantee behind the bounded history
    /// capture that replaces the full readback + full diff on pen-up.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn stroke_dirty_bounds_patch_matches_full_diff() {
        use crate::brush_engine::Dab;
        use crate::history::{LayerPatch, PatchBounds};

        let size = Size::new(128, 96);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        let red = Color::new(255, 0, 0);
        let before_full = canvas.read_layer(0).expect("read before");

        canvas.begin_stroke(red, 1.0, false).expect("begin");
        let dabs = [
            Dab::round(Point::new(30.0, 40.0), 9.0, red),
            Dab::round(Point::new(70.0, 50.0), 6.0, red),
        ];
        canvas.stamp(|t| t.paint_dabs(&dabs)).expect("stamp");

        let (bx, by, bw, bh) = canvas.stroke_dirty_bounds().expect("dirty bounds");

        canvas.commit_stroke().expect("commit");
        let after_full = canvas.read_layer(0).expect("read after");

        // Canonical patch from a full-canvas diff.
        let full_patch =
            LayerPatch::from_full_diff(&before_full, &after_full, size.width, size.height)
                .expect("stroke changed pixels");

        // The dab-quad dirty rect must be a superset of the true AABB.
        assert!(bx <= full_patch.bounds.x, "left {bx} > {}", full_patch.bounds.x);
        assert!(by <= full_patch.bounds.y, "top {by} > {}", full_patch.bounds.y);
        assert!(
            bx + bw >= full_patch.bounds.x + full_patch.bounds.w,
            "right {} < {}",
            bx + bw,
            full_patch.bounds.x + full_patch.bounds.w
        );
        assert!(
            by + bh >= full_patch.bounds.y + full_patch.bounds.h,
            "bottom {} < {}",
            by + bh,
            full_patch.bounds.y + full_patch.bounds.h
        );

        // A patch built from only the dirty region must be byte-identical
        // to the canonical full-diff patch.
        let region = PatchBounds {
            x: bx,
            y: by,
            w: bw,
            h: bh,
        };
        let before_region = LayerPatch::crop_canvas_region(&before_full, size.width, region);
        let after_region = LayerPatch::crop_canvas_region(&after_full, size.width, region);
        let region_patch = LayerPatch::from_region_diff(
            &before_region,
            &after_region,
            region,
            size.width,
            size.height,
        )
        .expect("region changed");

        assert_eq!(region_patch.bounds.x, full_patch.bounds.x);
        assert_eq!(region_patch.bounds.y, full_patch.bounds.y);
        assert_eq!(region_patch.bounds.w, full_patch.bounds.w);
        assert_eq!(region_patch.bounds.h, full_patch.bounds.h);
        assert_eq!(region_patch.before, full_patch.before);
        assert_eq!(region_patch.after, full_patch.after);
    }
}
