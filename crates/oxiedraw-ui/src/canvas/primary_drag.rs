//! Primary-button drag handling: brush, crop, and transform tools all
//! share one `gtk::GestureDrag` so GTK's GestureSingle mutual exclusion
//! doesn't silently deny one of them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_core::brush_engine::{BrushEngine, InputSample};
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::canvas::fill::{FillResult, flood_fill, paint_indices};
use oxiedraw_core::color::{Color, ColorState};
use oxiedraw_core::document::LayerKind;
use oxiedraw_core::history::{HistoryAction, HistoryStack, LayerPatch, PatchBounds, SelectionSnapshot};
use oxiedraw_core::selection::{RectShape, SelectionShape};
use oxiedraw_core::shape_correction::{CorrectedShape, corrected_samples, detect_shape};
use oxiedraw_core::text::{ResizeMode, TextBox};
use oxiedraw_core::tools::{
    CropHandle, CropRect, CropState, FillState, FillTool, PendingMarquee, SelectionMode,
    SelectionState, SelectionTool, ShapeState, ShapeTool, Tool, ToolState, TransformFilter,
    TransformHandle, TransformState,
};
use oxiedraw_utils::geometry::{Point, Size, TransformRect, morph_path};
use relm4::gtk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::canvas_paintable::CanvasPaintable;
use crate::settings::AppSettings;

use super::Viewport;
use super::{
    BUTTON_PRIMARY, crop_geom, present_into_paintable, sample_from,
    transform_geometry, widget_to_canvas,
};

/// History capture state for an in-flight brush stroke. Build-up brushes
/// modify the layer mid-stroke, so `before_full` holds a pristine
/// full-layer snapshot taken at stroke start. Other brushes leave it
/// `None` and read just the dirty region at pen-up (pre-commit), since the
/// layer is untouched until commit.
struct PendingStroke {
    idx: usize,
    id: String,
    before_full: Option<Vec<u8>>,
}

/// The data needed to finalize an in-flight shape-correction stroke to its
/// final corrected geometry. Held while the correction animation plays so
/// that a pen-up, or an undo/redo, can commit + record the corrected shape
/// immediately instead of leaving it unrecorded (or frozen mid-morph).
struct PendingCorrection {
    color: Color,
    opacity: f32,
    erase: bool,
    /// Original samples (pen dynamics preserved) remapped onto the corrected
    /// path - i.e. the stroke at animation end (`t == 1`).
    final_samples: Vec<InputSample>,
}

pub(super) struct PrimaryDragHandler {
    // -- shared ----------------------------------------------------------
    canvas: Rc<RefCell<Canvas>>,
    paintable: CanvasPaintable,
    pan: Rc<Cell<Point>>,
    zoom: Rc<Cell<f32>>,
    canvas_size: Rc<Cell<Size>>,
    tools: ToolState,
    area: gtk::Picture,
    /// Set when a drag handler has mutated the canvas and a present is owed.
    /// A one-shot frame-clock tick coalesces a burst of pointer-rate motion
    /// events into a single GPU composite + dmabuf publish per displayed
    /// frame (see [`Self::request_present`]).
    present_scheduled: Rc<Cell<bool>>,
    /// For the "can't draw on a component layer" notification.
    toaster: crate::toaster::Toaster,
    // -- brush -----------------------------------------------------------
    brush_engine: BrushEngine,
    colors: ColorState,
    stroke_points: Rc<RefCell<Vec<InputSample>>>,
    pending_color: Rc<Cell<Color>>,
    pending_opacity: Rc<Cell<f32>>,
    pending_erase: Rc<Cell<bool>>,
    pending_timer: Rc<RefCell<Option<glib::SourceId>>>,
    /// Set once a shape-correction animation is armed; lets pen-up and
    /// undo/redo finalize the corrected shape. `None` for a plain freehand
    /// stroke or once the correction has been committed.
    pending_correction: Rc<RefCell<Option<PendingCorrection>>>,
    // -- crop ------------------------------------------------------------
    crop: CropState,
    crop_handle: Rc<Cell<CropHandle>>,
    crop_start: Rc<Cell<Point>>,
    crop_start_rect: Rc<Cell<Option<CropRect>>>,
    // -- transform --------------------------------------------------------
    transform: TransformState,
    transform_handle: Rc<Cell<TransformHandle>>,
    transform_drag_start_canvas: Rc<Cell<Point>>,
    transform_drag_start_rect: Rc<Cell<Option<TransformRect>>>,
    /// Angle from rect centre to drag-start cursor (used for rotate handle).
    transform_drag_start_rotation_angle: Rc<Cell<f32>>,
    // -- selection --------------------------------------------------------
    selection: SelectionState,
    selection_drag_start: Rc<Cell<Point>>,
    // -- fill -------------------------------------------------------------
    fill: FillState,
    /// Active fill-animation timer, if any. Cancelled whenever a new
    /// fill begins so back-to-back clicks don't fight over the layer.
    fill_anim: Rc<RefCell<Option<glib::SourceId>>>,
    // -- shapes -----------------------------------------------------------
    shape: ShapeState,
    shape_drag_start: Rc<Cell<Point>>,
    /// Effective bounding box of the in-flight shape `(x, y, w, h)` in canvas
    /// pixels, updated on every drag move so `shape_end` can commit it.
    shape_cur_rect: Rc<Cell<Option<(f32, f32, f32, f32)>>>,
    /// Layer pixels + id captured at shape_begin for the history patch.
    shape_pending: Rc<RefCell<Option<ShapePending>>>,
    // -- text -------------------------------------------------------------
    /// Canvas-space point where a text drag/click started.
    text_drag_start: Rc<Cell<Point>>,
    /// In-flight text drag box `(x, y, w, h)` in canvas pixels; `None` until
    /// the pointer moves, which is how a click is told apart from a drag.
    text_cur_rect: Rc<Cell<Option<(f32, f32, f32, f32)>>>,
    /// `true` when the current gesture was consumed by the editor (caret
    /// placement / selection drag) rather than creating a new box.
    text_editing_gesture: Rc<Cell<bool>>,
    /// The on-canvas text editing controller (owns enter/exit/keys/render).
    text_edit: crate::text_edit::TextEdit,
    // --- cursor tool ---
    cursor_activates_transform: Rc<dyn Fn()>,
    // --- history ---
    history: Rc<RefCell<HistoryStack>>,
    /// Captured at brush_begin so brush_end can build a LayerPatch bounded
    /// to the stroke's dirty region.
    pending_capture: Rc<RefCell<Option<PendingStroke>>>,
}

/// State captured when a shape drag begins: which layer, its id, and
/// the pristine pixels (for the undo before-state). Selection clipping
/// happens on the GPU via the bound selection mask sampler.
struct ShapePending {
    idx: usize,
    id: String,
    before: Vec<u8>,
}

impl PrimaryDragHandler {
    // -- top-level dispatch ------------------------------------------------

    /// Whether the active layer is a (non-editable) component instance.
    fn active_is_component(&self) -> bool {
        let c = self.canvas.borrow();
        c.layers()
            .active()
            .and_then(|i| c.layers().kind(i))
            .is_some_and(|k| matches!(k, LayerKind::Component(_)))
    }

    /// Whether the active layer is a text layer (raster ops are rejected; it is
    /// re-rendered from its content and would clobber any painting).
    fn active_is_text(&self) -> bool {
        let c = self.canvas.borrow();
        c.layers()
            .active()
            .and_then(|i| c.layers().kind(i))
            .is_some_and(|k| matches!(k, LayerKind::Text(_)))
    }

    fn on_begin(&self, gesture: &gtk::GestureDrag, x: f64, y: f64) {
        // Raster tools are rejected on component layers - those are pre-rendered
        // and only editable by opening the component.
        let raster_tool = matches!(
            self.tools.active.get(),
            Tool::Brush | Tool::Fill(_) | Tool::Shapes(_)
        );
        if raster_tool && self.active_is_component() {
            self.toaster
                .info("Can't edit a component layer. Double-click it to open the component.");
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        if raster_tool && self.active_is_text() {
            self.toaster
                .info("Can't paint on a text layer. Rasterize it first.");
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        match self.tools.active.get() {
            Tool::Brush => self.brush_begin(gesture, x, y),
            Tool::Crop => self.crop_begin(x, y),
            Tool::Transform => self.transform_begin(x, y),
            Tool::Selection(s) => self.selection_begin(gesture, s, x, y),
            Tool::Fill(FillTool::Bucket) => {
                self.fill_begin(x, y);
                gesture.set_state(gtk::EventSequenceState::Denied);
            }
            Tool::Shapes(kind) => self.shape_begin(kind, x, y),
            Tool::ColorPicker => self.color_pick_begin(x, y),
            Tool::Text => self.text_begin(x, y),
            Tool::Cursor => {
                (self.cursor_activates_transform)();
                gesture.set_state(gtk::EventSequenceState::Denied);
            }
            _ => {
                gesture.set_state(gtk::EventSequenceState::Denied);
            }
        }
    }

    fn on_update(&self, gesture: &gtk::GestureDrag, dx: f64, dy: f64) {
        match self.tools.active.get() {
            Tool::Brush => self.brush_update(gesture, dx, dy),
            Tool::Crop => self.crop_update(gesture, dx, dy),
            Tool::Transform => self.transform_update(gesture, dx, dy),
            Tool::Selection(s) => self.selection_update(gesture, s, dx, dy),
            Tool::Shapes(kind) => self.shape_update(gesture, kind, dx, dy),
            Tool::ColorPicker => self.color_pick_update(),
            Tool::Text => self.text_update(gesture, dx, dy),
            _ => {}
        }
    }

    fn on_end(&self) {
        match self.tools.active.get() {
            Tool::Brush => self.brush_end(),
            Tool::Crop => self.crop_end(),
            Tool::Selection(s) => self.selection_end(s),
            Tool::Shapes(kind) => self.shape_end(kind),
            Tool::Text => self.text_end(),
            _ => {}
        }
    }

    // -- brush -------------------------------------------------------------

    fn brush_begin(&self, gesture: &gtk::GestureDrag, x: f64, y: f64) {
        // Discard any leftover idle timer from a prior stroke (safety net).
        if let Some(src) = self.pending_timer.borrow_mut().take() {
            src.remove();
        }
        *self.pending_correction.borrow_mut() = None;

        let color = self.colors.current();
        let opacity = self.brush_engine.opacity.get();
        let buildup = self.brush_engine.active_brush().buildup;
        let erase = self.tools.eraser.get();

        // Capture context so the idle timer can re-draw with the same settings.
        self.pending_color.set(color);
        self.pending_opacity.set(opacity);
        self.pending_erase.set(erase);
        self.stroke_points.borrow_mut().clear();

        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom);
        let sample = sample_from(gesture, canvas_pos);
        self.stroke_points.borrow_mut().push(sample);

        let mut canvas = self.canvas.borrow_mut();

        // History capture. Build-up brushes flush into the layer on every
        // move, so snapshot the pristine full layer now. Other brushes
        // leave the layer untouched until commit, so we defer to pen-up and
        // read only the (bounded) dirty region there - no full readback.
        *self.pending_capture.borrow_mut() = canvas.layers().active().and_then(|idx| {
            let id = canvas.layers().snapshot().get(idx).map(|l| l.id.clone())?;
            let before_full = if buildup {
                match canvas.read_layer(idx) {
                    Ok(before) => Some(before),
                    Err(e) => {
                        tracing::warn!(error = %e, "history: before-snapshot read failed");
                        return None;
                    }
                }
            } else {
                None
            };
            Some(PendingStroke {
                idx,
                id,
                before_full,
            })
        });

        if let Err(e) = canvas.begin_stroke(color, opacity, erase) {
            tracing::error!(error = %e, "canvas.begin_stroke failed");
            return;
        }
        if let Err(e) = canvas.stamp(|target| {
            self.brush_engine.begin_stroke(sample, color, target);
        }) {
            tracing::error!(error = %e, "stamp begin_stroke failed");
        }
        // Build-up brushes composite each step into the layer so repeated
        // dabs accumulate opacity; other brushes just accrue in the stroke
        // buffer. The present itself is coalesced to the next frame tick.
        if buildup && let Err(e) = canvas.flush_stroke() {
            tracing::error!(error = %e, "flush_stroke failed");
        }
        drop(canvas);
        self.request_present();
    }

    fn brush_update(&self, gesture: &gtk::GestureDrag, dx: f64, dy: f64) {
        // Skip if the brush engine already finished its stroke (shape correction
        // ends the engine's stroke in-place while the pen is still held).
        if !self.brush_engine.is_drawing() {
            return;
        }
        let Some((sx, sy)) = gesture.start_point() else {
            return;
        };
        let canvas_pos = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom);
        let sample = sample_from(gesture, canvas_pos);

        // Record the full sample (position + pen dynamics) for shape detection
        // and so correction can remap pressure/tilt/rotation across the path.
        self.stroke_points.borrow_mut().push(sample);

        // Reset the 2 s idle timer - it fires only when movement stops.
        self.reset_idle_timer();

        let buildup = self.brush_engine.active_brush().buildup;
        let mut canvas = self.canvas.borrow_mut();
        if let Err(e) = canvas.stamp(|target| {
            self.brush_engine.push_sample(sample, target);
        }) {
            tracing::error!(error = %e, "stamp push_sample failed");
        }
        if buildup && let Err(e) = canvas.flush_stroke() {
            tracing::error!(error = %e, "flush_stroke failed");
        }
        drop(canvas);
        self.request_present();
    }

    /// Coalesce canvas presents to one per frame-clock tick. Pointer-rate
    /// motion events (125-1000 Hz on a tablet) can fire several times per
    /// displayed frame; presenting on each one re-composites the whole
    /// canvas and re-imports the dmabuf for frames that are never shown.
    /// That wasted GPU work is invisible when the GPU is idle but pushes us
    /// past the vblank deadline under contention (e.g. an OBS PipeWire
    /// capture forcing full compositor composition), halving the effective
    /// frame rate. Instead, the hot paths stamp into the stroke buffer and
    /// call this; the actual `present()` runs once on the next tick. Stamps
    /// accumulate in the stroke buffer, so the single present shows every
    /// dab from the burst.
    fn request_present(&self) {
        // A tick is already pending; the burst collapses into it.
        if self.present_scheduled.replace(true) {
            return;
        }
        let canvas = Rc::clone(&self.canvas);
        let paintable = self.paintable.clone();
        let area = self.area.clone();
        let scheduled = Rc::clone(&self.present_scheduled);
        self.area.add_tick_callback(move |_area, _clock| {
            scheduled.set(false);
            present_into_paintable(&mut canvas.borrow_mut(), &paintable, &area);
            glib::ControlFlow::Break
        });
    }

    fn brush_end(&self) {
        // User lifted the pen - cancel any pending shape-correction idle timer.
        if let Some(src) = self.pending_timer.borrow_mut().take() {
            src.remove();
        }

        // A shape-correction animation was still playing: snap straight to the
        // final corrected shape and record that, instead of committing whatever
        // half-morphed frame happens to be in the buffer.
        if let Some(c) = self.pending_correction.borrow_mut().take() {
            let mut canvas = self.canvas.borrow_mut();
            if draw_corrected_into_buffer(
                &mut canvas,
                &self.brush_engine,
                c.color,
                c.opacity,
                c.erase,
                &c.final_samples,
            ) {
                commit_stroke_and_record(&mut canvas, &self.pending_capture, &self.history);
            } else {
                let _ = canvas.discard_stroke();
                let _ = self.pending_capture.borrow_mut().take();
            }
            present_into_paintable(&mut canvas, &self.paintable, &self.area);
            return;
        }

        let mut canvas = self.canvas.borrow_mut();
        // end_stroke is a no-op if shape correction already ended the engine stroke.
        if let Err(e) = canvas.stamp(|target| {
            self.brush_engine.end_stroke(target);
        }) {
            tracing::error!(error = %e, "stamp end_stroke failed");
        }

        let pending = self.pending_capture.borrow_mut().take();
        // Dirty rect of everything stamped this stroke (any brush). Bounds
        // both the readback and the diff so a small dab costs small work,
        // not a full-canvas readback + scan.
        let bounds = canvas.stroke_dirty_bounds();

        // Non-build-up strokes leave the layer pristine until commit, so
        // capture the before-region now (pre-commit). Build-up strokes
        // already hold a full snapshot from begin.
        let before_region = match (pending.as_ref(), bounds) {
            (Some(p), Some((x, y, w, h))) if p.before_full.is_none() => {
                let mut buf = Vec::new();
                match canvas.read_layer_region_into(p.idx, x, y, w, h, &mut buf) {
                    Ok(()) => Some(buf),
                    Err(e) => {
                        tracing::warn!(error = %e, "history: before-region read failed");
                        None
                    }
                }
            }
            _ => None,
        };

        // Always commit immediately on pen up - no deferred correction here.
        if let Err(e) = canvas.commit_stroke() {
            tracing::error!(error = %e, "commit_stroke failed");
        }

        if let (Some(p), Some((x, y, w, h))) = (pending, bounds) {
            self.record_stroke_history(&mut canvas, &p, (x, y, w, h), before_region);
        }

        present_into_paintable(&mut canvas, &self.paintable, &self.area);
    }

    /// Land any in-flight brush stroke immediately as a recorded history
    /// entry. Used by undo/redo (which fire while the pen may still be held)
    /// so a pending shape correction can't desync the canvas from the undo
    /// stack. Reuses `brush_end`, which already commits the corrected shape
    /// (animation in flight) or the freehand stroke (idle timer armed).
    fn finalize_pending_brush(&self) {
        if self.pending_correction.borrow().is_some() || self.brush_engine.is_drawing() {
            self.brush_end();
        }
    }

    /// Record a brush stroke into history as a tight `LayerPatch` over the
    /// stroke's dirty rect. `before_region` is the pre-commit region read
    /// (non-build-up); for build-up strokes it is `None` and the region is
    /// cropped from the full pre-stroke snapshot instead.
    fn record_stroke_history(
        &self,
        canvas: &mut Canvas,
        pending: &PendingStroke,
        rect: (u32, u32, u32, u32),
        before_region: Option<Vec<u8>>,
    ) {
        let (x, y, w, h) = rect;
        let region = PatchBounds { x, y, w, h };
        let cs = canvas.size();

        let mut after_region = Vec::new();
        if let Err(e) = canvas.read_layer_region_into(pending.idx, x, y, w, h, &mut after_region) {
            tracing::warn!(error = %e, "history: after-region read failed");
            return;
        }

        let before_region = match before_region {
            Some(buf) => buf,
            None => match &pending.before_full {
                Some(full) => LayerPatch::crop_canvas_region(full, cs.width, region),
                None => return,
            },
        };
        if before_region.len() != after_region.len() {
            tracing::warn!("history: stroke region size mismatch - skipping");
            return;
        }

        if let Some(patch) =
            LayerPatch::from_region_diff(&before_region, &after_region, region, cs.width, cs.height)
        {
            self.history.borrow_mut().record(HistoryAction::Stroke {
                layer_id: pending.id.clone(),
                patch,
            });
        }
    }

    /// Cancel any in-flight idle timer and start a fresh idle timer whose
    /// duration comes from the current `trigger_delay_ms` setting.
    fn reset_idle_timer(&self) {
        if let Some(src) = self.pending_timer.borrow_mut().take() {
            src.remove();
        }

        let sc = AppSettings::load().shape_correction;
        if !sc.enabled {
            return;
        }
        // Build-up mode flushes each stamp into the layer as it happens,
        // so shape correction can't unwind the stroke - skip the timer.
        if self.brush_engine.active_brush().buildup {
            return;
        }

        let canvas_t = Rc::clone(&self.canvas);
        let paintable_t = self.paintable.clone();
        let area_t = self.area.clone();
        let brush_engine_t = self.brush_engine.clone();
        let samples = self.stroke_points.borrow().clone();
        let color = self.pending_color.get();
        let opacity = self.pending_opacity.get();
        let erase = self.pending_erase.get();
        let timer_handle = Rc::clone(&self.pending_timer);
        let correction_handle = Rc::clone(&self.pending_correction);
        let capture_handle = Rc::clone(&self.pending_capture);
        let history_handle = Rc::clone(&self.history);

        let src = glib::timeout_add_local(
            std::time::Duration::from_millis(u64::from(sc.trigger_delay_ms)),
            move || {
                *timer_handle.borrow_mut() = None;

                if !brush_engine_t.is_drawing() {
                    return glib::ControlFlow::Break;
                }

                let positions: Vec<Point> = samples.iter().map(|s| s.position).collect();
                let Some(shape) = detect_shape(&positions) else {
                    return glib::ControlFlow::Break;
                };

                // Discard if the detected shape type is disabled.
                let shape_enabled = match &shape {
                    CorrectedShape::Line { .. } => sc.correct_line,
                    CorrectedShape::Circle { .. } => sc.correct_circle,
                    CorrectedShape::Rectangle { .. } => sc.correct_rectangle,
                };
                if !shape_enabled {
                    return glib::ControlFlow::Break;
                }

                // Stamp the freehand tail dabs so the buffer is complete.
                {
                    let mut canvas_ref = canvas_t.borrow_mut();
                    if let Err(e) = canvas_ref.stamp(|t| {
                        brush_engine_t.end_stroke(t);
                    }) {
                        tracing::error!(error = %e, "correction: end freehand failed");
                        return glib::ControlFlow::Break;
                    }
                }

                // Keep the original sample stream (count, timing, pen
                // dynamics) and only move each sample onto the corrected
                // shape at its matching arc-length position. This preserves
                // the temporal density the brush engine relies on for smooth
                // speed/pressure, so the corrected stroke isn't lumpy.
                let corrected_geo = corrected_samples(&shape);
                if corrected_geo.is_empty() {
                    return glib::ControlFlow::Break;
                }
                let corrected_pts = morph_path(&positions, &corrected_geo);

                // Stash the final corrected geometry so a pen-up or an
                // undo/redo can land the shape immediately instead of waiting
                // on (or losing) the animation.
                let final_samples: Vec<InputSample> = samples
                    .iter()
                    .zip(corrected_pts.iter())
                    .map(|(s, &target)| InputSample {
                        position: target,
                        ..*s
                    })
                    .collect();
                *correction_handle.borrow_mut() = Some(PendingCorrection {
                    color,
                    opacity,
                    erase,
                    final_samples,
                });

                let anim_src = start_shape_animation(
                    Rc::clone(&canvas_t),
                    paintable_t.clone(),
                    area_t.clone(),
                    brush_engine_t.clone(),
                    color,
                    opacity,
                    erase,
                    samples.clone(),
                    corrected_pts,
                    Rc::clone(&timer_handle),
                    Rc::clone(&correction_handle),
                    Rc::clone(&capture_handle),
                    Rc::clone(&history_handle),
                    sc.animation_speed_ms,
                );
                *timer_handle.borrow_mut() = Some(anim_src);

                glib::ControlFlow::Break
            },
        );

        *self.pending_timer.borrow_mut() = Some(src);
    }

    // -- crop -------------------------------------------------------------

    fn crop_begin(&self, x: f64, y: f64) {
        let pan = self.pan.get();
        let zoom = self.zoom.get();
        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom);
        let rect = self.crop.rect.get();

        let rect_widget = rect.map(|r| {
            let n = r.normalized();
            (
                pan.x + n.x * zoom,
                pan.y + n.y * zoom,
                pan.x + n.right() * zoom,
                pan.y + n.bottom() * zoom,
            )
        });
        #[allow(clippy::cast_possible_truncation)]
        let h = crop_geom::hit_test_widget(rect_widget, x as f32, y as f32);
        self.crop_handle.set(h);
        self.crop_start.set(canvas_pos);
        self.crop_start_rect.set(rect);
    }

    fn crop_update(&self, gesture: &gtk::GestureDrag, dx: f64, dy: f64) {
        let Some((sx, sy)) = gesture.start_point() else {
            return;
        };
        let pan = self.pan.get();
        let zoom = self.zoom.get();
        #[allow(clippy::cast_possible_truncation)]
        let cx = ((sx + dx) as f32 - pan.x) / zoom;
        #[allow(clippy::cast_possible_truncation)]
        let cy = ((sy + dy) as f32 - pan.y) / zoom;
        let sc = self.crop_start.get();
        let old = self.crop_start_rect.get();

        let new_rect = crop_geom::compute_new_rect(self.crop_handle.get(), old, sc, cx, cy);
        let new_rect = crop_geom::constrain_rect(
            new_rect,
            self.crop.aspect_ratio.get(),
            self.crop_handle.get(),
        );
        let new_rect = if self.crop.snap_to_canvas.get() {
            new_rect
                .map(|r| crop_geom::snap_rect_to_canvas(r, self.canvas_size.get(), self.zoom.get()))
        } else {
            new_rect
        };

        self.crop.rect.set(new_rect);
        self.crop.notify_rect_changed();
        self.paintable.set_crop(new_rect, self.crop.overlay.get());
    }

    fn crop_end(&self) {
        if let Some(r) = self.crop.rect.get() {
            let norm = r.normalized();
            let final_rect = if norm.w < 2.0 || norm.h < 2.0 {
                None
            } else {
                Some(norm)
            };
            self.crop.rect.set(final_rect);
            self.crop.notify_rect_changed();
            self.paintable.set_crop(final_rect, self.crop.overlay.get());
        }
    }

    // -- transform ---------------------------------------------------------

    fn transform_begin(&self, x: f64, y: f64) {
        let Some(rect) = self.transform.rect.get() else {
            return;
        };
        #[allow(clippy::cast_possible_truncation)]
        let handle = transform_geometry::hit_test(rect, x as f32, y as f32, &self.pan, &self.zoom);
        self.transform_handle.set(handle);
        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom);
        self.transform_drag_start_canvas.set(canvas_pos);
        self.transform_drag_start_rect.set(Some(rect));
        if handle == TransformHandle::Rotate {
            let a = (canvas_pos.y - rect.cy).atan2(canvas_pos.x - rect.cx);
            self.transform_drag_start_rotation_angle.set(a);
        }
    }

    fn transform_update(&self, gesture: &gtk::GestureDrag, dx: f64, dy: f64) {
        let Some((sx, sy)) = gesture.start_point() else {
            return;
        };
        let cur = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom);
        let Some(start_rect) = self.transform_drag_start_rect.get() else {
            return;
        };
        let start_canvas = self.transform_drag_start_canvas.get();
        let (shift, alt) = modifiers_from_gesture(gesture);
        let new_rect = transform_geometry::compute_rect(
            self.transform_handle.get(),
            start_rect,
            start_canvas,
            self.transform_drag_start_rotation_angle.get(),
            cur,
            shift,
            alt,
        );
        self.transform.rect.set(Some(new_rect));
        self.transform.notify_changed();
        self.paintable.set_transform_rect(Some(new_rect));
    }

    // -- selection ---------------------------------------------------------

    fn selection_begin(
        &self,
        gesture: &gtk::GestureDrag,
        tool: SelectionTool,
        x: f64,
        y: f64,
    ) {
        let mode = selection_mode_from_modifiers(gesture);
        self.selection.mode.set(mode);
        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom);
        self.selection_drag_start.set(canvas_pos);
        let initial = match tool {
            SelectionTool::Square | SelectionTool::Circle => Some(PendingMarquee::Rect {
                x: canvas_pos.x,
                y: canvas_pos.y,
                w: 0.0,
                h: 0.0,
                circle: matches!(tool, SelectionTool::Circle),
            }),
            SelectionTool::Free => Some(PendingMarquee::Lasso(vec![canvas_pos])),
        };
        *self.selection.pending.borrow_mut() = initial;
        self.selection.notify_changed();
        self.paintable.set_selection_pending(self.selection.pending.borrow().clone());
    }

    fn selection_update(
        &self,
        gesture: &gtk::GestureDrag,
        tool: SelectionTool,
        dx: f64,
        dy: f64,
    ) {
        let Some((sx, sy)) = gesture.start_point() else {
            return;
        };
        let cur = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom);
        match tool {
            SelectionTool::Square | SelectionTool::Circle => {
                let start = self.selection_drag_start.get();
                let new_pending = PendingMarquee::Rect {
                    x: start.x,
                    y: start.y,
                    w: cur.x - start.x,
                    h: cur.y - start.y,
                    circle: matches!(tool, SelectionTool::Circle),
                };
                *self.selection.pending.borrow_mut() = Some(new_pending);
            }
            SelectionTool::Free => {
                if let Some(PendingMarquee::Lasso(pts)) =
                    self.selection.pending.borrow_mut().as_mut()
                {
                    // Only append points that moved enough to be visually
                    // distinct - avoids piling up thousands of near-duplicates
                    // on a slow drag.
                    let last = pts.last().copied().unwrap_or(cur);
                    let dx2 = (cur.x - last.x).powi(2) + (cur.y - last.y).powi(2);
                    if dx2 > 1.0 {
                        pts.push(cur);
                    }
                }
            }
        }
        self.selection.notify_changed();
        self.paintable.set_selection_pending(self.selection.pending.borrow().clone());
    }

    fn selection_end(&self, _tool: SelectionTool) {
        let pending = self.selection.pending.borrow_mut().take();
        self.paintable.set_selection_pending(None);
        let Some(p) = pending else {
            return;
        };
        let mode = self.selection.mode.get();

        // Detect a click without a meaningful drag. In Replace mode a
        // click on empty area (or anywhere, really) clears the existing
        // selection - matches Photoshop. For Add/Subtract/Intersect we
        // leave the existing selection alone since a tiny no-op shape
        // can't change the mask sensibly.
        let is_click = match &p {
            PendingMarquee::Rect { w, h, .. } => w.abs() < 1.0 || h.abs() < 1.0,
            PendingMarquee::Lasso(pts) => pts.len() < 3,
        };
        if is_click {
            if mode == SelectionMode::Replace {
                self.deselect_via_click();
            } else {
                self.selection.notify_changed();
            }
            return;
        }

        let shape = match p {
            PendingMarquee::Rect {
                x,
                y,
                w,
                h,
                circle,
            } => {
                let rect = RectShape { x, y, w, h }.normalize();
                if circle {
                    SelectionShape::Ellipse(rect)
                } else {
                    SelectionShape::Rect(rect)
                }
            }
            PendingMarquee::Lasso(mut pts) => {
                // Close the polygon implicitly - rasteriser wraps last->first.
                // De-dup the final point if it duplicates the first one.
                if let (Some(&first), Some(&last)) = (pts.first(), pts.last())
                    && (first.x - last.x).abs() < 0.5 && (first.y - last.y).abs() < 0.5 {
                        pts.pop();
                    }
                SelectionShape::Polygon(pts)
            }
        };

        // Read before state for history.
        let before_sel = {
            let mut c = self.canvas.borrow_mut();
            if c.selection_active() {
                c.read_selection_mask().map_or(
                    SelectionSnapshot { active: true, mask: None },
                    |m| SelectionSnapshot { active: true, mask: Some(m) },
                )
            } else {
                SelectionSnapshot { active: false, mask: None }
            }
        };

        // Commit to the GPU mask.
        {
            let mut canvas = self.canvas.borrow_mut();
            if let Err(e) = canvas.apply_selection_shape(&shape, mode) {
                tracing::error!(error = %e, "apply_selection_shape failed");
                self.selection.notify_changed();
                return;
            }
            self.selection.active.set(canvas.selection_active());
        }

        // Read after state and record history.
        let after_sel = {
            let mut c = self.canvas.borrow_mut();
            if c.selection_active() {
                c.read_selection_mask().map_or(
                    SelectionSnapshot { active: true, mask: None },
                    |m| SelectionSnapshot { active: true, mask: Some(m) },
                )
            } else {
                SelectionSnapshot { active: false, mask: None }
            }
        };
        self.history.borrow_mut().record(HistoryAction::SelectionChange {
            before: before_sel,
            after: after_sel,
        });

        // A drag-drawn selection is layer-agnostic; drop any layer-binding
        // from a previous preview-click so Transform reverts to acting on
        // the active layer.
        self.selection.source_layer.set(None);
        refresh_selection_contours(&self.canvas, &self.selection, &self.canvas_size);
        self.selection.notify_changed();
        self.refresh_after_selection_change();
    }

    /// After the selection mask changes, re-present the canvas so any
    /// brush previews / cached textures pick up the new clip, and ask
    /// the paintable to redraw its overlay.
    fn refresh_after_selection_change(&self) {
        let mut canvas = self.canvas.borrow_mut();
        present_into_paintable(&mut canvas, &self.paintable, &self.area);
    }

    // -- fill -------------------------------------------------------------

    /// Click handler for the Bucket Fill tool. Reads the active layer,
    /// runs a flood fill from the clicked pixel, then animates the
    /// result outward from the seed like spilled paint.
    fn fill_begin(&self, x: f64, y: f64) {
        // Cancel any in-flight fill animation before starting a new one
        // so back-to-back clicks don't both write to the layer.
        if let Some(src) = self.fill_anim.borrow_mut().take() {
            src.remove();
        }

        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom);
        #[allow(clippy::cast_possible_truncation)]
        let sx = canvas_pos.x.floor() as i32;
        #[allow(clippy::cast_possible_truncation)]
        let sy = canvas_pos.y.floor() as i32;
        let cs = self.canvas_size.get();
        if sx < 0 || sy < 0 || (sx as u32) >= cs.width || (sy as u32) >= cs.height {
            return;
        }

        let (layer_idx, original, layer_id, selection_mask) = {
            let mut canvas = self.canvas.borrow_mut();
            let Some(idx) = canvas.layers().active() else {
                return;
            };
            let id = canvas.layers().snapshot()
                .get(idx).map(|l| l.id.clone()).unwrap_or_default();
            // Confine the fill to the active selection, if any.
            let mask = if canvas.selection_active() {
                match canvas.read_selection_mask() {
                    Ok(m) => Some(m),
                    Err(e) => {
                        tracing::error!(error = %e, "fill: read_selection_mask failed");
                        return;
                    }
                }
            } else {
                None
            };
            match canvas.read_layer(idx) {
                Ok(px) => (idx, px, id, mask),
                Err(e) => {
                    tracing::error!(error = %e, "fill: read_layer failed");
                    return;
                }
            }
        };

        let tolerance = self.fill.tolerance.get();
        let primary = self.colors.current();
        let color_bgr = [primary.b, primary.g, primary.r];
        let w = cs.width;
        let h = cs.height;

        // Run the BFS on a background thread so the UI stays
        // responsive while the flood fill computes - at 8k canvas
        // this is the difference between a 1.5s freeze and a
        // smooth-but-pending click. The worker owns the pixel
        // buffer during BFS and ships it back via the channel so
        // we never clone the 256MB buffer.
        let (tx, rx) = std::sync::mpsc::channel::<(Vec<u8>, Option<FillResult>)>();
        let primary_color = primary;
        std::thread::spawn(move || {
            let result = flood_fill(&original, w, h, sx, sy, tolerance, selection_mask.as_deref());
            let _ = tx.send((original, result));
        });

        let canvas_for_poll = Rc::clone(&self.canvas);
        let paintable_for_poll = self.paintable.clone();
        let area_for_poll = self.area.clone();
        let anim_for_poll = Rc::clone(&self.fill_anim);
        let tools_for_poll = self.tools.clone();
        let history_for_poll = Rc::clone(&self.history);
        let layer_id_for_poll = layer_id.clone();
        let poll_src = glib::timeout_add_local(
            std::time::Duration::from_millis(16),
            move || match rx.try_recv() {
                Ok((pixels, opt_result)) => {
                    // If the user has switched tools while the BFS
                    // worker was running, abort silently - they're
                    // no longer expecting a fill to land.
                    if !matches!(tools_for_poll.active.get(), Tool::Fill(FillTool::Bucket)) {
                        *anim_for_poll.borrow_mut() = None;
                        let _ = pixels;
                        let _ = opt_result;
                        return glib::ControlFlow::Break;
                    }
                    // Worker handed us its buffer + the BFS result.
                    // Clear the poll handle here so the animation
                    // installer can replace it with the animation
                    // timer below.
                    *anim_for_poll.borrow_mut() = None;
                    let Some(result) = opt_result else {
                        return glib::ControlFlow::Break;
                    };
                    if result.sorted_indices.is_empty() {
                        return glib::ControlFlow::Break;
                    }
                    if result.sorted_indices.len() == 1 {
                        let before_buf = pixels.clone();
                        let mut buffer = pixels;
                        paint_indices(&mut buffer, &result.sorted_indices, color_bgr);
                        let mut c = canvas_for_poll.borrow_mut();
                        if let Err(e) = c.restore_layer(layer_idx, &buffer) {
                            tracing::error!(error = %e, "fill: restore_layer failed");
                        }
                        drop(c);
                        let cs = canvas_for_poll.borrow().size();
                        if let Some(patch) = LayerPatch::from_full_diff(
                            &before_buf, &buffer, cs.width, cs.height,
                        ) {
                            history_for_poll.borrow_mut().record(HistoryAction::Fill {
                                layer_id: layer_id_for_poll.clone(),
                                patch,
                            });
                        }
                        present_into_paintable(
                            &mut canvas_for_poll.borrow_mut(),
                            &paintable_for_poll,
                            &area_for_poll,
                        );
                        return glib::ControlFlow::Break;
                    }
                    start_fill_animation(
                        Rc::clone(&canvas_for_poll),
                        paintable_for_poll.clone(),
                        area_for_poll.clone(),
                        layer_idx,
                        pixels,
                        result,
                        color_bgr,
                        primary_color,
                        Rc::clone(&anim_for_poll),
                        Rc::clone(&history_for_poll),
                        layer_id_for_poll.clone(),
                    );
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
            },
        );
        *self.fill_anim.borrow_mut() = Some(poll_src);
    }

    // -- shapes ------------------------------------------------------------

    fn shape_begin(&self, _kind: ShapeTool, x: f64, y: f64) {
        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom);
        self.shape_drag_start.set(canvas_pos);
        self.shape_cur_rect.set(None);

        // Capture the target layer's pristine pixels for the undo patch.
        // The selection clip is sampled directly by the shape shader from
        // the GPU mask - no need to read it back to CPU here.
        let pending = {
            let mut canvas = self.canvas.borrow_mut();
            let Some(idx) = canvas.layers().active() else {
                *self.shape_pending.borrow_mut() = None;
                return;
            };
            let id = canvas.layers().snapshot()
                .get(idx).map(|l| l.id.clone()).unwrap_or_default();
            match canvas.read_layer(idx) {
                Ok(before) => Some(ShapePending { idx, id, before }),
                Err(e) => {
                    tracing::error!(error = %e, "shape: read_layer failed");
                    None
                }
            }
        };
        *self.shape_pending.borrow_mut() = pending;

        // Arm the GPU shape overlay so subsequent presents splice the
        // shape into the preview image at the target layer's z-order.
        if let Some(p) = self.shape_pending.borrow().as_ref() {
            self.canvas.borrow_mut().begin_shape_overlay(p.idx);
        }
    }

    fn shape_update(&self, gesture: &gtk::GestureDrag, kind: ShapeTool, dx: f64, dy: f64) {
        let Some((sx, sy)) = gesture.start_point() else {
            return;
        };
        let cur = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom);
        let (shift, alt) = modifiers_from_gesture(gesture);
        let rect = oxiedraw_core::shapes::shape_rect_from_drag(
            self.shape_drag_start.get(),
            cur,
            kind,
            shift,
            alt,
        );
        self.shape_cur_rect.set(Some(rect));

        // Per-frame work is now a push-constant update + present - no
        // CPU rasterisation, no texture upload.
        let antialias = matches!(self.shape.filter.get(), TransformFilter::Bilinear);
        let line_width = oxiedraw_core::shapes::DEFAULT_LINE_WIDTH;
        let renderer_kind = kind.to_renderer_kind();
        let shader_rect = renderer_kind.pack_drag_rect(rect);
        let mut canvas = self.canvas.borrow_mut();
        canvas.set_shape_preview_params(
            renderer_kind,
            shader_rect,
            self.colors.current(),
            antialias,
            line_width,
        );
        present_into_paintable(&mut canvas, &self.paintable, &self.area);
    }

    fn shape_end(&self, kind: ShapeTool) {
        let Some((x, y, w, h)) = self.shape_cur_rect.take() else {
            // No drag occurred - drop overlay and pending state.
            self.canvas.borrow_mut().cancel_shape_overlay();
            self.shape_pending.borrow_mut().take();
            present_into_paintable(&mut self.canvas.borrow_mut(), &self.paintable, &self.area);
            return;
        };
        let Some(pending) = self.shape_pending.borrow_mut().take() else {
            self.canvas.borrow_mut().cancel_shape_overlay();
            return;
        };

        // Ignore a click / zero-area drag.
        let line = matches!(kind, ShapeTool::Line);
        if (!line && (w.abs() < 1.0 || h.abs() < 1.0))
            || (line && w.abs() < 1.0 && h.abs() < 1.0)
        {
            self.canvas.borrow_mut().cancel_shape_overlay();
            present_into_paintable(&mut self.canvas.borrow_mut(), &self.paintable, &self.area);
            return;
        }

        let antialias = matches!(self.shape.filter.get(), TransformFilter::Bilinear);
        let line_width = oxiedraw_core::shapes::DEFAULT_LINE_WIDTH;
        let renderer_kind = kind.to_renderer_kind();
        let shader_rect = renderer_kind.pack_drag_rect((x, y, w, h));

        let cs = {
            let mut canvas = self.canvas.borrow_mut();
            if let Err(e) = canvas.commit_shape(
                pending.idx,
                renderer_kind,
                shader_rect,
                self.colors.current(),
                antialias,
                line_width,
            ) {
                tracing::error!(error = %e, "shape: commit_shape failed");
                // commit_shape clears the overlay even on failure; repaint
                // so the stale preview frame doesn't linger on screen.
                canvas.cancel_shape_overlay();
                drop(canvas);
                present_into_paintable(&mut self.canvas.borrow_mut(), &self.paintable, &self.area);
                return;
            }
            canvas.size()
        };

        // History: tight diff of before vs after layer pixels.
        let after = match self.canvas.borrow_mut().read_layer(pending.idx) {
            Ok(px) => px,
            Err(e) => {
                tracing::error!(error = %e, "shape: read_layer after commit failed");
                present_into_paintable(&mut self.canvas.borrow_mut(), &self.paintable, &self.area);
                return;
            }
        };
        if let Some(patch) =
            LayerPatch::from_full_diff(&pending.before, &after, cs.width, cs.height)
        {
            self.history.borrow_mut().record(HistoryAction::Shape {
                layer_id: pending.id,
                patch,
            });
        }

        present_into_paintable(&mut self.canvas.borrow_mut(), &self.paintable, &self.area);
    }

    /// Selection-tool click with no drag: clear any existing selection.
    /// Photoshop parity - a single click in the marquee/lasso tool means
    /// "I'm not picking anything, drop the current selection".
    fn deselect_via_click(&self) {
        let before_mask = {
            let mut c = self.canvas.borrow_mut();
            if c.selection_active() {
                c.read_selection_mask().ok()
            } else {
                None
            }
        };
        {
            let mut canvas = self.canvas.borrow_mut();
            canvas.deselect();
            self.selection.active.set(false);
        }
        self.history.borrow_mut().record(HistoryAction::SelectionChange {
            before: SelectionSnapshot { active: before_mask.is_some(), mask: before_mask },
            after: SelectionSnapshot { active: false, mask: None },
        });
        self.selection.ants_contours.borrow_mut().clear();
        self.selection.source_layer.set(None);
        self.selection.notify_changed();
        self.refresh_after_selection_change();
    }

    // -- color picker ------------------------------------------------------

    fn color_pick_begin(&self, x: f64, y: f64) {
        // The loupe (driven by the motion controller) has normally already
        // sampled this pixel on hover; reuse that. Fall back to a direct read
        // for a click on a spot the loupe hasn't sampled yet.
        let color = self.paintable.picker_color().or_else(|| {
            let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom);
            super::sample_canvas_color(&self.canvas, canvas_pos)
        });
        self.commit_picked_color(color);
    }

    fn color_pick_update(&self) {
        // Motion fires alongside each drag update and refreshes the loupe's
        // sample, so commit that rather than issuing a second GPU readback.
        self.commit_picked_color(self.paintable.picker_color());
    }

    /// Store a picked color in the active slot and notify the picker widget
    /// to redraw. Dragging re-commits on every move so the selected color
    /// tracks the pointer.
    fn commit_picked_color(&self, color: Option<Color>) {
        if let Some(color) = color {
            self.colors.set_current(color);
            self.colors.notify_changed();
        }
    }

    // -- text --------------------------------------------------------------

    /// Smallest drag (canvas px) on either axis that counts as a box drag
    /// rather than a click.
    const TEXT_DRAG_MIN: f32 = 3.0;

    fn text_begin(&self, x: f64, y: f64) {
        let start = widget_to_canvas(x, y, &self.pan, &self.zoom);
        self.text_drag_start.set(start);
        self.text_cur_rect.set(None);
        // If the editor consumed the press (caret placement inside an active
        // box, or entering an existing text layer), this gesture edits text;
        // otherwise it will create a new box on release. Clicking empty space
        // while already editing only dismisses that edit - it must not spawn a
        // new box, so treat it as an editing gesture too.
        let was_editing = self.text_edit.is_active();
        let consumed = self.text_edit.pointer_press(start);
        self.text_editing_gesture.set(consumed || was_editing);
    }

    fn text_update(&self, gesture: &gtk::GestureDrag, dx: f64, dy: f64) {
        let Some((sx, sy)) = gesture.start_point() else {
            return;
        };
        let cur = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom);
        if self.text_editing_gesture.get() {
            self.text_edit.pointer_motion(cur);
            return;
        }
        let start = self.text_drag_start.get();
        let (x, y, w, h) = (start.x, start.y, cur.x - start.x, cur.y - start.y);
        self.text_cur_rect.set(Some((x, y, w, h)));

        // Live rubber-band outline of the box being dragged out.
        let nx = if w >= 0.0 { x } else { x + w };
        let ny = if h >= 0.0 { y } else { y + h };
        let (nw, nh) = (w.abs(), h.abs());
        self.paintable.set_text_pending_box(Some(TransformRect::new(
            nx + nw / 2.0,
            ny + nh / 2.0,
            nw,
            nh,
            0.0,
        )));
    }

    fn text_end(&self) {
        self.paintable.set_text_pending_box(None);
        if self.text_editing_gesture.get() {
            self.text_edit.pointer_release();
            return;
        }
        let start = self.text_drag_start.get();
        let (box_rect, mode) = match self.text_cur_rect.take() {
            Some((x, y, w, h)) if w.abs() >= Self::TEXT_DRAG_MIN && h.abs() >= Self::TEXT_DRAG_MIN => {
                // Drag: a fixed-size box of the dragged rect.
                let nx = if w >= 0.0 { x } else { x + w };
                let ny = if h >= 0.0 { y } else { y + h };
                let nw = w.abs();
                let nh = h.abs();
                (
                    TextBox::new(nx + nw / 2.0, ny + nh / 2.0, nw, nh, 0.0),
                    ResizeMode::Fixed,
                )
            }
            // Click (or negligible drag): auto-width box anchored at the click.
            _ => (TextBox::new(start.x, start.y, 0.0, 0.0, 0.0), ResizeMode::AutoWidth),
        };
        self.text_edit.create_and_edit(box_rect, mode);
    }
}

/// Recompute the marching-ants contours from the current selection mask
/// and store them on the paintable. Reads the full-resolution mask and
/// runs a pixel-perfect axis-aligned boundary tracer so the contour
/// follows every pixel transition exactly (no diagonal smoothing).
pub(crate) fn refresh_selection_contours(
    canvas: &Rc<RefCell<Canvas>>,
    selection: &SelectionState,
    canvas_size: &Rc<Cell<Size>>,
) {
    let (mask, mw, mh) = {
        let mut c = canvas.borrow_mut();
        if !c.selection_active() {
            selection.ants_contours.borrow_mut().clear();
            return;
        }
        let size = canvas_size.get();
        match c.read_selection_mask() {
            Ok(m) => (m, size.width, size.height),
            Err(err) => {
                tracing::error!(error = %err, "read_selection_mask failed");
                return;
            }
        }
    };
    let contours = oxiedraw_core::selection::pixel_perfect_contours(&mask, mw, mh);
    *selection.ants_contours.borrow_mut() = contours;
}

/// Read the `(shift, alt)` modifier state from a drag gesture.
fn modifiers_from_gesture(gesture: &gtk::GestureDrag) -> (bool, bool) {
    let modifiers = gesture
        .current_event().map_or_else(gtk::gdk::ModifierType::empty, |e| e.modifier_state());
    (
        modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK),
        modifiers.contains(gtk::gdk::ModifierType::ALT_MASK),
    )
}

fn selection_mode_from_modifiers(gesture: &gtk::GestureDrag) -> SelectionMode {
    let modifiers = gesture
        .current_event().map_or_else(gtk::gdk::ModifierType::empty, |e| e.modifier_state());
    let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
    let alt = modifiers.contains(gtk::gdk::ModifierType::ALT_MASK);
    match (shift, alt) {
        (true, true) => SelectionMode::Intersect,
        (true, false) => SelectionMode::Add,
        (false, true) => SelectionMode::Subtract,
        (false, false) => SelectionMode::Replace,
    }
}

/// Redraw `samples` (the corrected stroke at its final geometry) into a fresh
/// stroke buffer. Leaves the buffer ready to commit; the layer stays pristine.
/// Returns false if any renderer call failed (caller should discard).
fn draw_corrected_into_buffer(
    canvas: &mut Canvas,
    brush_engine: &BrushEngine,
    color: Color,
    opacity: f32,
    erase: bool,
    samples: &[InputSample],
) -> bool {
    if let Err(e) = canvas.discard_stroke() {
        tracing::error!(error = %e, "correction: discard_stroke failed");
        return false;
    }
    if let Err(e) = canvas.begin_stroke(color, opacity, erase) {
        tracing::error!(error = %e, "correction: begin_stroke failed");
        return false;
    }
    for (i, &sample) in samples.iter().enumerate() {
        let result = if i == 0 {
            canvas.stamp(|t| brush_engine.begin_stroke(sample, color, t))
        } else {
            canvas.stamp(|t| brush_engine.push_sample(sample, t))
        };
        if let Err(e) = result {
            tracing::error!(error = %e, "correction: stamp failed");
            return false;
        }
    }
    if let Err(e) = canvas.stamp(|t| brush_engine.end_stroke(t)) {
        tracing::error!(error = %e, "correction: end_stroke stamp failed");
        return false;
    }
    true
}

/// Commit the in-flight stroke buffer into its layer and record it as a tight
/// `HistoryAction::Stroke` over the committed dirty rect. Used by the
/// shape-correction path: the layer is pristine until this commit, so the
/// pre-commit region read is exactly the undo "before" state. Consumes
/// `pending_capture` so the pen-up handler won't record the same stroke twice.
fn commit_stroke_and_record(
    canvas: &mut Canvas,
    pending_capture: &Rc<RefCell<Option<PendingStroke>>>,
    history: &Rc<RefCell<HistoryStack>>,
) {
    let bounds = canvas.stroke_dirty_bounds();
    let pending = pending_capture.borrow_mut().take();
    let (Some(p), Some((x, y, w, h))) = (pending, bounds) else {
        if let Err(e) = canvas.commit_stroke() {
            tracing::error!(error = %e, "correction: commit_stroke failed");
        }
        return;
    };

    let mut before = Vec::new();
    let before_ok = match canvas.read_layer_region_into(p.idx, x, y, w, h, &mut before) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "history: corrected before-region read failed");
            false
        }
    };

    if let Err(e) = canvas.commit_stroke() {
        tracing::error!(error = %e, "correction: commit_stroke failed");
    }
    if !before_ok {
        return;
    }

    let mut after = Vec::new();
    if let Err(e) = canvas.read_layer_region_into(p.idx, x, y, w, h, &mut after) {
        tracing::warn!(error = %e, "history: corrected after-region read failed");
        return;
    }
    if before.len() != after.len() {
        tracing::warn!("history: corrected region size mismatch - skipping");
        return;
    }

    let cs = canvas.size();
    let region = PatchBounds { x, y, w, h };
    if let Some(patch) = LayerPatch::from_region_diff(&before, &after, region, cs.width, cs.height) {
        history
            .borrow_mut()
            .record(HistoryAction::Stroke { layer_id: p.id, patch });
    }
}

fn start_shape_animation(
    canvas: Rc<RefCell<Canvas>>,
    paintable: CanvasPaintable,
    area: gtk::Picture,
    brush_engine: BrushEngine,
    color: Color,
    opacity: f32,
    erase: bool,
    freehand_samples: Vec<InputSample>,
    corrected_pts: Vec<Point>,
    pending_timer: Rc<RefCell<Option<glib::SourceId>>>,
    pending_correction: Rc<RefCell<Option<PendingCorrection>>>,
    pending_capture: Rc<RefCell<Option<PendingStroke>>>,
    history: Rc<RefCell<HistoryStack>>,
    animation_speed_ms: u32,
) -> glib::SourceId {
    let frame = Rc::new(Cell::new(0_u32));
    const TOTAL_FRAMES: u32 = 20;
    // Per-frame interval: spread total duration evenly; minimum 1 ms.
    let frame_ms = u64::from((animation_speed_ms / TOTAL_FRAMES).max(1));

    glib::timeout_add_local(std::time::Duration::from_millis(frame_ms), move || {
        let f = frame.get();
        #[allow(clippy::cast_precision_loss)]
        let t_raw = f as f32 / (TOTAL_FRAMES - 1) as f32;
        let t_ease = 1.0 - (1.0 - t_raw.min(1.0)).powi(3);

        // Morph each sample's position toward the corrected path while keeping
        // its captured pen dynamics, so pressure/tilt/rotation ride along the
        // whole corrected stroke instead of snapping back to defaults.
        let interpolated: Vec<InputSample> = freehand_samples
            .iter()
            .zip(corrected_pts.iter())
            .map(|(s, &target)| InputSample {
                position: s.position.lerp(target, t_ease),
                ..*s
            })
            .collect();

        // Discard previous frame's stroke and redraw with interpolated positions.
        let mut canvas_ref = canvas.borrow_mut();
        if let Err(e) = canvas_ref.discard_stroke() {
            tracing::error!(error = %e, "animation: discard_stroke failed");
            *pending_timer.borrow_mut() = None;
            return glib::ControlFlow::Break;
        }
        if let Err(e) = canvas_ref.begin_stroke(color, opacity, erase) {
            tracing::error!(error = %e, "animation: begin_stroke failed");
            *pending_timer.borrow_mut() = None;
            return glib::ControlFlow::Break;
        }

        // Drive the brush engine through all interpolated samples.
        let mut stamp_ok = true;
        for (i, &sample) in interpolated.iter().enumerate() {
            let result = if i == 0 {
                canvas_ref.stamp(|t| {
                    brush_engine.begin_stroke(sample, color, t);
                })
            } else {
                canvas_ref.stamp(|t| {
                    brush_engine.push_sample(sample, t);
                })
            };
            if let Err(e) = result {
                tracing::error!(error = %e, "animation: stamp failed");
                stamp_ok = false;
                break;
            }
        }

        if stamp_ok {
            // Flush trailing dabs from the engine.
            if let Err(e) = canvas_ref.stamp(|t| {
                brush_engine.end_stroke(t);
            }) {
                tracing::error!(error = %e, "animation: end_stroke stamp failed");
                stamp_ok = false;
            }
        }

        let is_last = f + 1 >= TOTAL_FRAMES;

        if is_last || !stamp_ok {
            // The corrected shape is now fully painted into the stroke buffer;
            // it is settled, so drop the finalizer state and record it.
            *pending_correction.borrow_mut() = None;
            if stamp_ok {
                commit_stroke_and_record(&mut canvas_ref, &pending_capture, &history);
            } else if let Err(e) = canvas_ref.discard_stroke() {
                tracing::error!(error = %e, "animation: discard after stamp failure");
            }
            present_into_paintable(&mut canvas_ref, &paintable, &area);
            *pending_timer.borrow_mut() = None;
            glib::ControlFlow::Break
        } else {
            present_into_paintable(&mut canvas_ref, &paintable, &area);
            frame.set(f + 1);
            glib::ControlFlow::Continue
        }
    })
}

/// Paint-spill animation for the bucket fill, driven on the GPU.
///
/// One-shot setup: upload the R8 distance mask + premultiplied colour
/// to the renderer's fill-overlay image, then arm the overlay path.
/// Per frame all that runs is a push-constant update (the reveal
/// radius) + the present pipeline - so a single animation tick is
/// effectively free at any canvas size. The layer itself stays
/// untouched until the final commit, when the actual filled BGRA8
/// pixels are uploaded and the overlay is cleared.
///
/// Timing is wall-clock driven (`Instant::elapsed`) so a slow frame
/// doesn't desync the spread.
#[allow(clippy::too_many_arguments)]
fn start_fill_animation(
    canvas: Rc<RefCell<Canvas>>,
    paintable: CanvasPaintable,
    area: gtk::Picture,
    layer_idx: usize,
    original: Vec<u8>,
    result: FillResult,
    color_bgr: [u8; 3],
    color: Color,
    anim_handle: Rc<RefCell<Option<glib::SourceId>>>,
    history: Rc<RefCell<HistoryStack>>,
    layer_id: String,
) {
    const FRAME_MS: u64 = 16;
    const DURATION_MS: u64 = 400;

    // Arm the GPU overlay: one mask upload + state setup.
    {
        let mut c = canvas.borrow_mut();
        if let Err(e) = c.begin_fill_overlay(layer_idx, &result.distance_mask, color) {
            tracing::error!(error = %e, "fill: begin_fill_overlay failed");
            // Fall through to a one-shot commit so the user still
            // sees the fill land instead of nothing.
            let before_buf = original.clone();
            let mut buf = original;
            paint_indices(&mut buf, &result.sorted_indices, color_bgr);
            if let Err(e) = c.restore_layer(layer_idx, &buf) {
                tracing::error!(error = %e, "fill: restore_layer fallback failed");
            }
            let cs = c.size();
            if let Some(patch) = LayerPatch::from_full_diff(&before_buf, &buf, cs.width, cs.height) {
                history.borrow_mut().record(HistoryAction::Fill { layer_id, patch });
            }
            present_into_paintable(&mut c, &paintable, &area);
            return;
        }
        present_into_paintable(&mut c, &paintable, &area);
    }

    let before_pixels = original.clone();
    let buffer = Rc::new(RefCell::new(original));
    let indices = Rc::new(result.sorted_indices);
    let start = std::time::Instant::now();
    let anim_handle_inner = Rc::clone(&anim_handle);
    let canvas_for_anim = Rc::clone(&canvas);

    let src = glib::timeout_add_local(std::time::Duration::from_millis(FRAME_MS), move || {
        #[allow(clippy::cast_possible_truncation)]
        let elapsed_ms = start.elapsed().as_millis() as u64;
        #[allow(clippy::cast_precision_loss)]
        let t = (elapsed_ms as f32 / DURATION_MS as f32).clamp(0.0, 1.0);
        // Ease-out cubic: fast at first, slows at the edges - feels
        // like ink/paint spreading and dragging at its boundary.
        let eased = 1.0 - (1.0 - t).powi(3);

        // The R8 mask stores 0..=254 across the fill region; cap the
        // reveal threshold just under the sentinel value so the
        // shader's `d > reveal` test admits every in-region pixel
        // exactly at eased == 1.0.
        let max_reveal = 254.0 / 255.0;
        let reveal = (eased * max_reveal).clamp(0.0, max_reveal);

        let done = elapsed_ms >= DURATION_MS;

        if done {
            // Final frame: bake the fill into the layer pixels and
            // tear down the overlay. The layer write is the one and
            // only big upload of the entire animation.
            paint_indices(&mut buffer.borrow_mut(), &indices, color_bgr);
            let mut c = canvas_for_anim.borrow_mut();
            let buf = buffer.borrow();
            if let Err(e) = c.commit_fill_overlay(layer_idx, &buf) {
                tracing::error!(error = %e, "fill: commit_fill_overlay failed");
            }
            let cs = c.size();
            if let Some(patch) = LayerPatch::from_full_diff(&before_pixels, &buf, cs.width, cs.height) {
                history.borrow_mut().record(HistoryAction::Fill { layer_id: layer_id.clone(), patch });
            }
            drop(buf);
            present_into_paintable(&mut c, &paintable, &area);
            *anim_handle_inner.borrow_mut() = None;
            glib::ControlFlow::Break
        } else {
            let mut c = canvas_for_anim.borrow_mut();
            c.set_fill_reveal(reveal);
            present_into_paintable(&mut c, &paintable, &area);
            glib::ControlFlow::Continue
        }
    });

    *anim_handle.borrow_mut() = Some(src);
}

pub(super) fn install_primary_drag(
    area: &gtk::Picture,
    viewport: &Viewport,
    brush_engine: &BrushEngine,
    colors: &ColorState,
    tools: &ToolState,
    crop: &CropState,
    transform: &TransformState,
    selection: &SelectionState,
    fill: &FillState,
    shape: &ShapeState,
    history: &Rc<RefCell<HistoryStack>>,
    toaster: &crate::toaster::Toaster,
    text_edit: &crate::text_edit::TextEdit,
    cursor_activates_transform: Rc<dyn Fn()>,
) {
    let handler = Rc::new(PrimaryDragHandler {
        canvas: Rc::clone(&viewport.canvas),
        paintable: viewport.paintable.clone(),
        pan: Rc::clone(&viewport.pan),
        zoom: Rc::clone(&viewport.zoom),
        canvas_size: Rc::clone(&viewport.canvas_size),
        tools: tools.clone(),
        area: area.clone(),
        present_scheduled: Rc::new(Cell::new(false)),
        toaster: toaster.clone(),
        brush_engine: brush_engine.clone(),
        colors: colors.clone(),
        stroke_points: Rc::new(RefCell::new(Vec::new())),
        pending_color: Rc::new(Cell::new(Color::new(0, 0, 0))),
        pending_opacity: Rc::new(Cell::new(1.0)),
        pending_erase: Rc::new(Cell::new(false)),
        pending_timer: Rc::new(RefCell::new(None)),
        pending_correction: Rc::new(RefCell::new(None)),
        crop: crop.clone(),
        crop_handle: Rc::new(Cell::new(CropHandle::None)),
        crop_start: Rc::new(Cell::new(Point::ZERO)),
        crop_start_rect: Rc::new(Cell::new(None)),
        transform: transform.clone(),
        transform_handle: Rc::new(Cell::new(TransformHandle::None)),
        transform_drag_start_canvas: Rc::new(Cell::new(Point::ZERO)),
        transform_drag_start_rect: Rc::new(Cell::new(None)),
        transform_drag_start_rotation_angle: Rc::new(Cell::new(0.0)),
        selection: selection.clone(),
        selection_drag_start: Rc::new(Cell::new(Point::ZERO)),
        fill: fill.clone(),
        fill_anim: Rc::new(RefCell::new(None)),
        shape: shape.clone(),
        shape_drag_start: Rc::new(Cell::new(Point::ZERO)),
        shape_cur_rect: Rc::new(Cell::new(None)),
        shape_pending: Rc::new(RefCell::new(None)),
        text_drag_start: Rc::new(Cell::new(Point::ZERO)),
        text_cur_rect: Rc::new(Cell::new(None)),
        text_editing_gesture: Rc::new(Cell::new(false)),
        text_edit: text_edit.clone(),
        cursor_activates_transform,
        history: Rc::clone(history),
        pending_capture: Rc::new(RefCell::new(None)),
    });

    let drag = gtk::GestureDrag::new();
    drag.set_button(BUTTON_PRIMARY);

    {
        let h = Rc::clone(&handler);
        drag.connect_drag_begin(move |g, x, y| h.on_begin(g, x, y));
    }
    {
        let h = Rc::clone(&handler);
        drag.connect_drag_update(move |g, dx, dy| h.on_update(g, dx, dy));
    }
    {
        let h = Rc::clone(&handler);
        drag.connect_drag_end(move |_, _, _| h.on_end());
    }

    // Let undo/redo land an in-flight shape correction before they touch
    // history, so the corrected shape is recorded rather than silently
    // overwriting an older action's undo state.
    {
        let h = Rc::clone(&handler);
        *viewport.flush_correction_handle().borrow_mut() =
            Some(Box::new(move || h.finalize_pending_brush()));
    }

    area.add_controller(drag);
}
