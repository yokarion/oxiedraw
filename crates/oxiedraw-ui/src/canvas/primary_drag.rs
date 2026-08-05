//! Primary-button drag handling: brush, crop, and transform tools all
//! share one `gtk::GestureDrag` so GTK's GestureSingle mutual exclusion
//! doesn't silently deny one of them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use oxiedraw_core::brush_engine::{BrushEngine, InputSample};
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::canvas::fill::{FillOptions, FillResult, FillSource, flood_fill, paint_fill};
use oxiedraw_core::color::{Color, ColorState};
use oxiedraw_core::document::LayerKind;
use oxiedraw_core::guides::{
    assist_lock, vp_default_color, AssistLock, GuideKind, GuideState, VanishingPoint,
};
use oxiedraw_core::history::{HistoryAction, HistoryStack, LayerPatch, PatchBounds, SelectionSnapshot};
use oxiedraw_core::selection::{RectShape, SelectionShape};
use oxiedraw_core::shape_correction::{ShapeKind, detect_correction};
use oxiedraw_core::text::{ResizeMode, TextBox};
use oxiedraw_core::tools::{
    CropHandle, CropRect, CropState, FillState, FillTool, GradientState, PendingMarquee,
    SelectionMode, SelectionState, SelectionTool, ShapeState, ShapeTool, TargetKind, Tool,
    ToolState, TransformFilter, TransformHandle, TransformState,
};
use oxiedraw_utils::geometry::{Point, Size, TransformRect};
use relm4::gtk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::canvas_paintable::CanvasPaintable;
use crate::settings::{AppSettings, ShapeCorrectionSettings};

use super::{RenderPump, Viewport};
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

/// Distance (screen px) from origin to the rotation node. Matches the overlay
/// constant in `canvas_paintable`.
const GUIDE_ROT_HANDLE_PX: f32 = 64.0;
/// Pointer-to-node grab radius in screen pixels (node radius + slack).
const GUIDE_NODE_HIT_PX: f32 = 14.0;
/// Screen-space deadzone before Drawing Assist locks a stroke to a guide line.
/// The lock direction is chosen from the drag once it clears this, so a short
/// nudge doesn't commit to the wrong axis (helpful, not twitchy).
const GUIDE_SNAP_LOCK_PX: f32 = 8.0;

/// Straight RGB (`0.0..=1.0` per channel) for a [`Color`], for seeding guide
/// colours from the primary colour.
fn color_to_rgb(c: Color) -> (f32, f32, f32) {
    (f32::from(c.r) / 255.0, f32::from(c.g) / 255.0, f32::from(c.b) / 255.0)
}

/// Which drawing-guide node a gesture is manipulating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum GuideDrag {
    #[default]
    None,
    Position,
    Rotation,
}

pub(super) struct PrimaryDragHandler {
    // -- shared ----------------------------------------------------------
    canvas: Rc<RefCell<Canvas>>,
    paintable: CanvasPaintable,
    pan: Rc<Cell<Point>>,
    zoom: Rc<Cell<f32>>,
    rotation: Rc<Cell<f32>>,
    canvas_size: Rc<Cell<Size>>,
    tools: ToolState,
    area: gtk::Picture,
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
    /// Shape-correction settings snapshotted at stroke start. Read on every
    /// motion event by the idle-timer reset; loading them from disk per event
    /// (a `read_to_string` + JSON parse) saturated the main thread at stylus
    /// report rates and jittered the frame clock. They can't change mid-stroke.
    stroke_shape_correction: RefCell<ShapeCorrectionSettings>,
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
    /// When the reveal sweep started, if one is running. Driven by the
    /// render pump's frame clock, not a timer, and deliberately outside
    /// the session so a gesture ending mid-sweep can't strand it.
    fill_reveal: Rc<Cell<Option<std::time::Instant>>>,
    /// Ticks once per fill gesture; see [`FillSession::generation`].
    fill_generation: Rc<Cell<u64>>,
    render_pump: RenderPump,
    /// The fill gesture in progress: alive from press to release so a
    /// sideways drag can re-run it at a new threshold.
    fill_session: Rc<RefCell<Option<FillSession>>>,
    // -- shapes -----------------------------------------------------------
    shape: ShapeState,
    shape_drag_start: Rc<Cell<Point>>,
    /// Effective bounding box of the in-flight shape `(x, y, w, h)` in canvas
    /// pixels, updated on every drag move so `shape_end` can commit it.
    shape_cur_rect: Rc<Cell<Option<(f32, f32, f32, f32)>>>,
    /// Layer pixels + id captured at shape_begin for the history patch.
    shape_pending: Rc<RefCell<Option<ShapePending>>>,
    // -- gradient ---------------------------------------------------------
    gradient: GradientState,
    /// Canvas-space point where the gradient drag started.
    gradient_drag_start: Rc<Cell<Point>>,
    /// Current endpoints `(x0, y0, x1, y1)` in canvas pixels, updated every
    /// drag move so `gradient_end` can commit them.
    gradient_cur_endpoints: Rc<Cell<Option<[f32; 4]>>>,
    /// Layer pixels + id captured at gradient_begin for the history patch.
    gradient_pending: Rc<RefCell<Option<GradientPending>>>,
    // -- drawing guide ----------------------------------------------------
    guide: GuideState,
    /// Which guide node the current gesture grabbed.
    guide_drag: Rc<Cell<GuideDrag>>,
    /// `(start pointer canvas pos, start origin, start angle)` captured at
    /// guide_begin so the update maps deltas onto the pre-drag config.
    guide_drag_start: Rc<Cell<Option<(Point, Point, f32)>>>,
    /// Index of the perspective vanishing point being dragged, if any.
    guide_vp_drag: Rc<Cell<Option<usize>>>,
    /// Canvas point where a tap (no drag) on empty space would add a new
    /// vanishing point. Cleared once the pointer moves past the node radius.
    guide_pending_add: Rc<Cell<Option<Point>>>,
    /// Stroke start when Drawing Assist snapping is armed for the current brush
    /// stroke (grid/isometric/perspective). `None` when the active guide doesn't
    /// snap - symmetry reproduces instead, and no guide leaves it unset.
    guide_snap_start: Rc<Cell<Option<Point>>>,
    /// The locked snap line once the pointer has moved past the deadzone. All
    /// further stroke points are projected onto it (straight guide line).
    guide_snap: Rc<RefCell<Option<AssistLock>>>,
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

/// State captured when a gradient drag begins: the target layer, its id,
/// and its pristine pixels for the undo before-state. Selection clipping is
/// sampled on the GPU from the bound selection mask.
struct GradientPending {
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
        // A fill gesture ends on the button coming up, which normally
        // arrives as a motion event - but a release outside the window
        // never delivers one, leaving the fill committed and unrecorded.
        // Closing it out before any other gesture records anything keeps
        // it in the right place on the undo stack.
        self.finalize_pending_fill();
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
            // The sequence is claimed, not denied: holding after the drop
            // and dragging sideways adjusts the threshold.
            Tool::Fill(FillTool::Bucket) => {
                tracing::debug!(source = ?event_source(gesture), "fill: drag begin");
                self.fill_begin(x, y);
            }
            Tool::Fill(FillTool::Gradient) => self.gradient_begin(x, y),
            Tool::Shapes(kind) => self.shape_begin(kind, x, y),
            Tool::ColorPicker => self.color_pick_begin(x, y),
            Tool::Text => self.text_begin(x, y),
            Tool::DrawingGuide => self.guide_begin(x, y),
            Tool::Cursor => {
                (self.cursor_activates_transform)();
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
            Tool::Fill(FillTool::Bucket) => self.fill_update(dx),
            Tool::Fill(FillTool::Gradient) => self.gradient_update(gesture, dx, dy),
            Tool::ColorPicker => self.color_pick_update(),
            Tool::Text => self.text_update(gesture, dx, dy),
            Tool::DrawingGuide => self.guide_update(gesture),
            _ => {}
        }
    }

    fn on_end(&self, gesture: &gtk::GestureDrag) {
        match self.tools.active.get() {
            Tool::Brush => self.brush_end(),
            Tool::Crop => self.crop_end(),
            Tool::Selection(s) => self.selection_end(s),
            Tool::Shapes(kind) => self.shape_end(kind),
            Tool::Fill(FillTool::Bucket) => {
                // Tablets end this gesture with the pen still on the
                // glass. Ending the fill here would cut the threshold
                // drag off mid-gesture, so leave it to the button: the
                // motion controller closes it out when that comes up.
                let held = gesture
                    .current_event_state()
                    .contains(gtk::gdk::ModifierType::BUTTON1_MASK);
                tracing::debug!(
                    held,
                    live = self.fill_session.borrow().is_some(),
                    "fill: drag end",
                );
                if !held {
                    self.fill_end();
                }
            }
            Tool::Fill(FillTool::Gradient) => self.gradient_end(),
            Tool::Text => self.text_end(),
            Tool::DrawingGuide => self.guide_end(),
            _ => {}
        }
    }

    // -- drawing guide -----------------------------------------------------

    fn guide_begin(&self, x: f64, y: f64) {
        let Some(cfg) = self.guide.config.borrow().clone() else {
            return;
        };
        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation);
        let zoom = self.zoom.get();
        // Node sizes are screen-fixed; convert to canvas units for hit-testing.
        let hit = (GUIDE_NODE_HIT_PX) / zoom;

        // Reset per-gesture state.
        self.guide_vp_drag.set(None);
        self.guide_pending_add.set(None);
        self.guide_drag.set(GuideDrag::None);
        self.guide_drag_start.set(None);

        // Perspective: grab a vanishing point to drag it, or arm a tap on empty
        // space to add a new one.
        if cfg.kind == GuideKind::Perspective {
            if let Some(i) = cfg
                .vanishing_points
                .iter()
                .position(|vp| canvas_pos.distance(vp.point()) <= hit)
            {
                self.guide_vp_drag.set(Some(i));
            } else {
                self.guide_pending_add.set(Some(canvas_pos));
            }
            return;
        }

        let rot_dist = GUIDE_ROT_HANDLE_PX / zoom;
        let (s, c) = cfg.angle.sin_cos();
        let rot_node = Point::new(cfg.origin.x + c * rot_dist, cfg.origin.y + s * rot_dist);

        let drag = if canvas_pos.distance(rot_node) <= hit {
            GuideDrag::Rotation
        } else {
            // Grabbing the position node or anywhere else moves the whole guide.
            GuideDrag::Position
        };
        self.guide_drag.set(drag);
        self.guide_drag_start.set(Some((canvas_pos, cfg.origin, cfg.angle)));
    }

    fn guide_update(&self, gesture: &gtk::GestureDrag) {
        let Some((sx, sy)) = gesture.start_point() else {
            return;
        };
        let Some((offset_x, offset_y)) = gesture.offset() else {
            return;
        };
        let cur = widget_to_canvas(sx + offset_x, sy + offset_y, &self.pan, &self.zoom, &self.rotation);

        // Perspective: drag a vanishing point, or cancel a pending tap-to-add
        // once the pointer has clearly moved (so a drag isn't read as a tap).
        if let Some(i) = self.guide_vp_drag.get() {
            self.guide.update(|c| {
                if let Some(vp) = c.vanishing_points.get_mut(i) {
                    vp.x = cur.x;
                    vp.y = cur.y;
                }
            });
            return;
        }
        if let Some(start) = self.guide_pending_add.get() {
            if cur.distance(start) > GUIDE_NODE_HIT_PX / self.zoom.get() {
                self.guide_pending_add.set(None);
            }
            return;
        }

        let Some((start_pos, start_origin, start_angle)) = self.guide_drag_start.get() else {
            return;
        };
        match self.guide_drag.get() {
            GuideDrag::Position => {
                let nx = start_origin.x + (cur.x - start_pos.x);
                let ny = start_origin.y + (cur.y - start_pos.y);
                self.guide.update(|c| c.origin = Point::new(nx, ny));
            }
            GuideDrag::Rotation => {
                let mut angle = (cur.y - start_origin.y).atan2(cur.x - start_origin.x);
                // Ctrl snaps to 15-degree increments, like canvas rotation.
                if gesture
                    .current_event_state()
                    .contains(gtk::gdk::ModifierType::CONTROL_MASK)
                {
                    let step = std::f32::consts::FRAC_PI_2 / 6.0;
                    angle = (angle / step).round() * step;
                }
                let _ = start_angle;
                self.guide.update(|c| c.angle = angle);
            }
            GuideDrag::None => {}
        }
    }

    fn guide_end(&self) {
        // A tap on empty space (still pending, i.e. not dragged away) adds a
        // vanishing point, up to Procreate's three-point maximum.
        if let Some(p) = self.guide_pending_add.take() {
            let primary = color_to_rgb(self.colors.current());
            self.guide.update(|c| {
                if c.kind == GuideKind::Perspective && c.vanishing_points.len() < 3 {
                    let color = vp_default_color(c.vanishing_points.len(), primary);
                    c.vanishing_points.push(VanishingPoint::new(p.x, p.y, color));
                }
            });
        }
        self.guide_vp_drag.set(None);
        self.guide_drag.set(GuideDrag::None);
        self.guide_drag_start.set(None);
    }

    // -- drawing assist (stroke snapping) ---------------------------------

    /// Arm stroke snapping for the guide in effect, if it snaps (grid /
    /// isometric / perspective with Assist on). `start` is the stroke anchor.
    fn arm_guide_snap(&self, start: Point) {
        *self.guide_snap.borrow_mut() = None;
        self.guide_snap_start.set(None);
        let cfg = self.guide.config.borrow();
        let snaps = cfg
            .as_ref()
            .is_some_and(oxiedraw_core::guides::GuideConfig::snaps_strokes);
        tracing::info!(
            target: "guide_assist",
            has_cfg = cfg.is_some(),
            kind = ?cfg.as_ref().map(|c| c.kind),
            assisted = ?cfg.as_ref().map(|c| c.assisted),
            snaps,
            "arm_guide_snap"
        );
        if snaps {
            self.guide_snap_start.set(Some(start));
        }
    }

    /// Map a raw stroke point through the armed guide snap. Before the deadzone
    /// is cleared it returns the point unchanged; once a drag direction is clear
    /// it locks onto the best-matching guide line and projects onto it.
    fn snap_guide_point(&self, raw: Point) -> Point {
        let Some(start) = self.guide_snap_start.get() else {
            return raw;
        };
        if let Some(lock) = *self.guide_snap.borrow() {
            return lock.project(raw);
        }
        if raw.distance(start) < GUIDE_SNAP_LOCK_PX / self.zoom.get() {
            return raw;
        }
        if let Some(cfg) = self.guide.config.borrow().as_ref()
            && let Some(lock) = assist_lock(cfg, start, raw)
        {
            tracing::info!(target: "guide_assist", dir = ?lock.dir, "snap locked");
            *self.guide_snap.borrow_mut() = Some(lock);
            return lock.project(raw);
        }
        raw
    }

    // -- brush -------------------------------------------------------------

    fn brush_begin(&self, gesture: &gtk::GestureDrag, x: f64, y: f64) {
        // Discard any leftover idle timer from a prior stroke (safety net).
        if let Some(src) = self.pending_timer.borrow_mut().take() {
            src.remove();
        }
        *self.pending_correction.borrow_mut() = None;
        // Snapshot shape-correction settings for the whole stroke so the
        // per-event idle-timer reset doesn't hit disk on every motion event.
        *self.stroke_shape_correction.borrow_mut() = AppSettings::load().shape_correction;

        let opacity = self.brush_engine.opacity.get();
        let active_brush = self.brush_engine.active_brush();
        let buildup = active_brush.buildup;
        // Colour-smudge brushes paint straight into the layer during the drag,
        // so the layer must be snapshotted before the first dab for undo.
        let smudge = active_brush.family.is_smudge();
        // Shape correction re-draws the stroke through the mask path at pen-up,
        // which doesn't apply to smudge - disable it for this stroke.
        if smudge {
            self.stroke_shape_correction.borrow_mut().enabled = false;
        }

        // Adjustment-layer masks are grayscale and can't be erased; mirror the
        // core invariant here so the live preview matches the committed stroke.
        let on_adjustment = {
            let canvas = self.canvas.borrow();
            canvas
                .layers()
                .active()
                .and_then(|idx| canvas.layers().kind(idx))
                .is_some_and(|k| k.is_adjustment())
        };
        let color = if on_adjustment {
            self.colors.current().to_grayscale()
        } else {
            self.colors.current()
        };
        let erase = !on_adjustment && self.tools.eraser.get();

        // Capture context so the idle timer can re-draw with the same settings.
        self.pending_color.set(color);
        self.pending_opacity.set(opacity);
        self.pending_erase.set(erase);
        self.stroke_points.borrow_mut().clear();

        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation);
        // Arm Drawing Assist snapping for this stroke (grid/iso/perspective);
        // the anchor point itself is never moved.
        self.arm_guide_snap(canvas_pos);
        let sample = sample_from(gesture, canvas_pos);
        self.stroke_points.borrow_mut().push(sample);

        let mut canvas = self.canvas.borrow_mut();

        // History capture. Every brush (build-up included) now leaves the
        // layer untouched until commit - build-up accumulates in the stroke
        // buffer via OVER-blend, not per-event flushes - so we defer to
        // pen-up and read only the (bounded) dirty region there.
        *self.pending_capture.borrow_mut() = canvas.layers().active().and_then(|idx| {
            let id = canvas.layers().snapshot().get(idx).map(|l| l.id.clone())?;
            Some(PendingStroke {
                idx,
                id,
                before_full: None,
            })
        });

        if let Err(e) = canvas.begin_stroke(color, opacity, erase) {
            tracing::error!(error = %e, "canvas.begin_stroke failed");
            return;
        }
        // Build-up accumulates coverage in the stroke buffer (OVER-blend) so
        // overlapping dabs darken, capped at the stroke opacity by the single
        // commit composite. The render pump presents the live buffer.
        canvas.set_stroke_buildup(buildup);
        // Route smudge brushes to the GPU smudge path. `set_smudge_stroke`
        // snapshots the pristine layer on the GPU (`smudge_before`); undo reads
        // just the dirty region back from it at pen-up, so there's no
        // full-canvas readback here at pen-down.
        if smudge && let Err(e) = canvas.set_smudge_stroke(true) {
            tracing::error!(error = %e, "set_smudge_stroke failed");
        }
        if let Err(e) = canvas.stamp(|target| {
            self.brush_engine.begin_stroke(sample, color, target);
        }) {
            tracing::error!(error = %e, "stamp begin_stroke failed");
        }
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
        let canvas_pos = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom, &self.rotation);
        // Drawing Assist: snap the point onto the active guide line (no-op when
        // no snapping guide is armed).
        let canvas_pos = self.snap_guide_point(canvas_pos);
        let sample = sample_from(gesture, canvas_pos);

        // Record the full sample (position + pen dynamics) for shape detection
        // and so correction can remap pressure/tilt/rotation across the path.
        self.stroke_points.borrow_mut().push(sample);

        // Reset the 2 s idle timer - it fires only when movement stops.
        self.reset_idle_timer();

        let mut canvas = self.canvas.borrow_mut();
        if let Err(e) = canvas.stamp(|target| {
            self.brush_engine.push_sample(sample, target);
        }) {
            tracing::error!(error = %e, "stamp push_sample failed");
        }
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
        // capture the before-region now (pre-commit). Smudge mutated the layer
        // live, so read its pristine before-state from the pre-stroke snapshot
        // instead of the (now-smudged) layer. Runs before `commit_stroke`,
        // which clears the smudge flag.
        let before_region = match (pending.as_ref(), bounds) {
            (Some(p), Some((x, y, w, h))) if p.before_full.is_none() => {
                let mut buf = Vec::new();
                let res = if canvas.is_smudge_stroke() {
                    canvas.read_smudge_before_region_into(x, y, w, h, &mut buf)
                } else {
                    canvas.read_layer_region_into(p.idx, x, y, w, h, &mut buf)
                };
                match res {
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

        // Drawing Assist already constrains the stroke to a straight guide line;
        // don't also run shape correction over it.
        if self.guide_snap_start.get().is_some() {
            return;
        }

        let sc = self.stroke_shape_correction.borrow().clone();
        if !sc.enabled {
            return;
        }
        // Shape correction is not offered for build-up brushes - their
        // overlap-accumulating look is the whole point, so snapping the
        // path to a clean shape would misrepresent the stroke.
        if self.brush_engine.active_brush().buildup {
            return;
        }

        let canvas_t = Rc::clone(&self.canvas);
        let paintable_t = self.paintable.clone();
        let area_t = self.area.clone();
        let brush_engine_t = self.brush_engine.clone();
        // Share the sample buffer (cheap Rc clone) and read it when the timer
        // actually fires, instead of deep-cloning the growing Vec every event.
        let stroke_points = Rc::clone(&self.stroke_points);
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

                // Read the full stroke now that movement has paused (the timer
                // only fires after `trigger_delay_ms` of no new samples).
                let samples = stroke_points.borrow().clone();
                let positions: Vec<Point> = samples.iter().map(|s| s.position).collect();
                let Some(correction) = detect_correction(&positions) else {
                    return glib::ControlFlow::Break;
                };

                // Discard if the detected shape type is disabled.
                let shape_enabled = match correction.kind {
                    ShapeKind::Line => sc.correct_line,
                    ShapeKind::Ellipse => sc.correct_circle,
                    ShapeKind::Rectangle => sc.correct_rectangle,
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

                // The correction target is already aligned 1:1 with the input
                // samples, so the original sample stream (count, timing, pen
                // dynamics) stays intact - each sample just moves onto its
                // corrected position. This preserves the temporal density the
                // brush engine relies on for smooth speed/pressure.
                let corrected_pts = correction.target;
                if corrected_pts.len() != positions.len() {
                    return glib::ControlFlow::Break;
                }

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
        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation);
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
        // The crop handles are stored in unrotated widget space (pan + canvas *
        // zoom); map the pointer into that same frame so hit-testing lands on
        // the handles under a rotated view.
        let hx = pan.x + canvas_pos.x * zoom;
        let hy = pan.y + canvas_pos.y * zoom;
        let h = crop_geom::hit_test_widget(rect_widget, hx, hy);
        self.crop_handle.set(h);
        self.crop_start.set(canvas_pos);
        self.crop_start_rect.set(rect);
    }

    fn crop_update(&self, gesture: &gtk::GestureDrag, dx: f64, dy: f64) {
        let Some((sx, sy)) = gesture.start_point() else {
            return;
        };
        // Rotation-aware inverse map, matching crop_begin and the other tools.
        let cur = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom, &self.rotation);
        let (cx, cy) = (cur.x, cur.y);
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
        // NB: deliberately do NOT call notify_rect_changed() here. It syncs the
        // numeric W/H spinner and label, and updating those widgets queues a
        // resize that GTK4 propagates up to the canvas Picture, re-allocating it
        // under the pen and cancelling the stylus grab mid-drag. The overlay
        // still tracks live via set_crop below; crop_end() syncs the numbers.
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
        let handle = transform_geometry::hit_test(rect, x as f32, y as f32, &self.pan, &self.zoom, &self.rotation);
        self.transform_handle.set(handle);
        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation);
        self.transform_drag_start_canvas.set(canvas_pos);
        self.transform_drag_start_rect.set(Some(rect));
        if handle == TransformHandle::Rotate {
            let a = (canvas_pos.y - rect.cy).atan2(canvas_pos.x - rect.cx);
            self.transform_drag_start_rotation_angle.set(a);
        }
        // Show the blended preview the moment the layer is grabbed, before any
        // movement, so a non-Normal layer doesn't flash Normal on press.
        self.update_transform_gpu_preview(rect);
    }

    fn transform_update(&self, gesture: &gtk::GestureDrag, dx: f64, dy: f64) {
        let Some((sx, sy)) = gesture.start_point() else {
            return;
        };
        let cur = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom, &self.rotation);
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

        self.update_transform_gpu_preview(new_rect);
    }

    /// Refresh the live GPU blend preview for `rect`: start it if needed (the
    /// GSK overlay can't show the layer's blend mode), then warp + blend +
    /// present through Vulkan. No-op when the GPU preview couldn't start.
    fn update_transform_gpu_preview(&self, rect: TransformRect) {
        self.ensure_transform_gpu_preview();
        // Text: re-render the warp source as the box grows to keep it crisp.
        self.refresh_text_transform_source(rect);
        // All targets share one affine; multi targets are canvas-sized, so the
        // first target's source dims drive the shared push.
        let dims = self.transform.targets.borrow().first().map(|t| t.src_dims);
        if let Some((sw, sh)) = dims
            && let Some(orig) = self.transform.original_rect.get()
        {
            let mut canvas = self.canvas.borrow_mut();
            if canvas.transform_preview_active() {
                canvas.set_transform_preview(orig, rect, sw, sh);
                present_into_paintable(&mut canvas, &self.paintable, &self.area);
            }
        }
    }

    /// Re-render a lone text layer's warp source at the current visible
    /// resolution once the box grows past it, so scaling stays crisp during the
    /// drag. Only the single-target case: a text layer inside a multi-selection
    /// warps its canvas pixels and re-renders crisply only at commit.
    fn refresh_text_transform_source(&self, rect: TransformRect) {
        let (idx, src_w, src_h) = {
            let targets = self.transform.targets.borrow();
            if targets.len() != 1 {
                return;
            }
            let t = &targets[0];
            if !matches!(t.kind, TargetKind::Text { .. }) {
                return;
            }
            (t.layer_idx, t.src_dims.0, t.src_dims.1)
        };
        #[allow(clippy::cast_precision_loss)]
        let (src_w_f, src_h_f) = (src_w as f32, src_h as f32);
        if rect.w.max(1.0) <= src_w_f + 1.0 && rect.h.max(1.0) <= src_h_f + 1.0 {
            return;
        }
        let Some(LayerKind::Text(content)) = self.canvas.borrow().layers().kind(idx) else {
            return;
        };
        let natural = content.box_rect;
        if natural.w.abs() <= 1e-3 || natural.h.abs() <= 1e-3 {
            return;
        }
        const HEADROOM: f32 = 1.5;
        let sx = rect.w / natural.w * HEADROOM;
        let sy = rect.h / natural.h * HEADROOM;
        let (pixels, sw, sh) = self.text_edit.render_scaled_source(&content, sx, sy);
        if pixels.len() != (sw as usize) * (sh as usize) * 4 {
            return;
        }
        #[allow(clippy::cast_precision_loss)]
        let orig_full = TransformRect::new(sw as f32 / 2.0, sh as f32 / 2.0, sw as f32, sh as f32, 0.0);
        {
            let mut canvas = self.canvas.borrow_mut();
            if canvas.begin_transform_preview_gpu(&[(idx, &pixels, sw, sh)]).is_err() {
                return;
            }
        }
        self.paintable.set_transform_gpu_preview(true);
        self.paintable.set_transform_source(Some(&pixels), sw, sh, Some(orig_full));
        self.transform.original_rect.set(Some(orig_full));
        if let Some(t) = self.transform.targets.borrow_mut().get_mut(0) {
            t.src_dims = (sw, sh);
            t.orig_bounds = orig_full;
            t.pixels = pixels;
        }
    }

    /// Begin the live GPU transform preview if it isn't already running, from the
    /// captured targets. Falls back to the GSK overlay on a dims/pixels mismatch.
    fn ensure_transform_gpu_preview(&self) {
        let mut canvas = self.canvas.borrow_mut();
        if canvas.transform_preview_active() {
            return;
        }
        let targets = self.transform.targets.borrow();
        if targets.is_empty() {
            return;
        }
        let mut sources: Vec<(usize, &[u8], u32, u32)> = Vec::with_capacity(targets.len());
        for t in targets.iter() {
            let (w, h) = t.src_dims;
            // Guard against a dims/pixels mismatch uploading garbage.
            if t.pixels.len() != (w as usize) * (h as usize) * 4 {
                return;
            }
            sources.push((t.layer_idx, t.pixels.as_slice(), w, h));
        }
        match canvas.begin_transform_preview_gpu(&sources) {
            Ok(()) => self.paintable.set_transform_gpu_preview(true),
            Err(e) => tracing::error!(error = %e, "begin_transform_preview_gpu failed"),
        }
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
        // Snap to the pixel grid so selection edges land on whole pixels.
        let canvas_pos = snap_to_pixel(widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation));
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
        let cur = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom, &self.rotation);
        match tool {
            SelectionTool::Square | SelectionTool::Circle => {
                let start = self.selection_drag_start.get();
                let cur = snap_to_pixel(cur);
                let (mut w, mut h) = (cur.x - start.x, cur.y - start.y);
                // Shift constrains to a 1:1 square/circle, keeping the drag
                // direction on each axis.
                let (shift, _) = modifiers_from_gesture(gesture);
                if shift {
                    let side = w.abs().max(h.abs());
                    w = side.copysign(w);
                    h = side.copysign(h);
                }
                let new_pending = PendingMarquee::Rect {
                    x: start.x,
                    y: start.y,
                    w,
                    h,
                    circle: matches!(tool, SelectionTool::Circle),
                };
                *self.selection.pending.borrow_mut() = Some(new_pending);
            }
            SelectionTool::Free => {
                let cur = snap_to_pixel(cur);
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

    /// Press handler for the Bucket Fill tool. Reads the active layer,
    /// runs a flood fill from the clicked pixel, commits it, then sweeps
    /// it into view from the seed like spilled paint.
    ///
    /// The buffers it collects stay alive for as long as the pointer is
    /// down: keep holding and drag sideways and the fill is recomputed
    /// at a new threshold, always from these pristine pixels.
    fn fill_begin(&self, x: f64, y: f64) {
        // Only the previous *animation* is dropped. Its pixels are
        // already committed, so a fast second click can never take the
        // first fill down with it.
        // Any previous gesture was already closed out by `on_begin`.
        self.end_fill_animation();

        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation);
        #[allow(clippy::cast_possible_truncation)]
        let sx = canvas_pos.x.floor() as i32;
        #[allow(clippy::cast_possible_truncation)]
        let sy = canvas_pos.y.floor() as i32;
        let cs = self.canvas_size.get();
        if sx < 0 || sy < 0 || (sx as u32) >= cs.width || (sy as u32) >= cs.height {
            return;
        }

        let sample_all_layers = self.fill.sample_all_layers.get();
        let (layer_idx, original, fill_source, occluders, layer_id, selection_mask) = {
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
            // When sampling all layers, the flood fill seeds/matches against
            // the composited canvas; the fill still paints into the active
            // layer, so we read that separately as the paint target. The
            // layers above it decide how the edge pixels are weighted -
            // line art on top blends the fill for us, line art below does
            // not - so read those too.
            let (source, above) = if sample_all_layers {
                let composite = match canvas.read_pixels() {
                    Ok(px) => px,
                    Err(e) => {
                        tracing::error!(error = %e, "fill: read_pixels failed");
                        return;
                    }
                };
                let above = match canvas.read_layers_above(idx) {
                    Ok(px) => Some(px),
                    Err(e) => {
                        tracing::error!(error = %e, "fill: read_layers_above failed");
                        None
                    }
                };
                (Some(composite), above)
            } else {
                (None, None)
            };
            match canvas.read_layer(idx) {
                Ok(px) => (idx, px, source, above, id, mask),
                Err(e) => {
                    tracing::error!(error = %e, "fill: read_layer failed");
                    return;
                }
            }
        };

        let primary = self.colors.current();
        let tolerance = self.fill.tolerance.get();
        let generation = self.fill_generation.get().wrapping_add(1);
        self.fill_generation.set(generation);
        *self.fill_session.borrow_mut() = Some(FillSession {
            generation,
            layer_idx,
            layer_id,
            before: Arc::new(original),
            sample: fill_source.map(Arc::new),
            occluders: occluders.map(Arc::new),
            mask: selection_mask.map(Arc::new),
            seed: (sx, sy),
            size: (cs.width, cs.height),
            color_bgr: [primary.b, primary.g, primary.r],
            auto_edge: self.fill.auto_edge.get(),
            origin_x: x,
            base_tolerance: tolerance,
            tolerance,
            current: None,
            in_flight: false,
            restart: false,
            released: false,
            animate: true,
        });
        fill_request(&self.fill_ctx());
    }

    /// The motion controller's view of the drag: an absolute widget
    /// position plus whether the button is still down.
    ///
    /// This is what actually drives and ends a fill gesture. The drag
    /// gesture can report its end while a pen is still on the tablet,
    /// and everything after that would otherwise be ignored - the fill
    /// would adjust for a moment and then go dead under the user's hand.
    fn fill_update_at(&self, x: f64, held: bool) {
        let origin = self.fill_session.borrow().as_ref().map(|s| s.origin_x);
        let Some(origin) = origin else {
            return;
        };
        if held {
            self.fill_update(x - origin);
        } else {
            tracing::debug!("fill: button released, ending gesture");
            self.fill_end();
        }
    }

    /// Drag handler: the pointer is still down after the drop, so
    /// sideways movement is a threshold adjustment. Right fills more,
    /// left fills less - the fill is recomputed and replaced in place.
    fn fill_update(&self, dx: f64) {
        // A tap is never perfectly still; nothing happens until the
        // pointer has clearly set off sideways.
        let travel = dx.abs() - TOLERANCE_DRAG_DEAD_ZONE_PX;
        if travel <= 0.0 {
            return;
        }
        let shift = travel.copysign(dx) * (255.0 / TOLERANCE_DRAG_RANGE_PX);

        let Some((tolerance, was_percent)) = ({
            let mut guard = self.fill_session.borrow_mut();
            let Some(s) = guard.as_mut() else {
                return;
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let next = (f64::from(s.base_tolerance) + shift).round().clamp(0.0, 255.0) as u8;
            (next != s.tolerance).then(|| {
                let was = percent(s.tolerance);
                s.tolerance = next;
                // Adjusting is a live edit - no point sweeping it in.
                s.animate = false;
                (next, was)
            })
        }) else {
            return;
        };

        self.end_fill_animation();
        // Plain cell write - the slider is only pushed once the gesture
        // ends, since touching another widget mid-drag is exactly the
        // kind of thing that unsettles a tablet's grab.
        self.fill.tolerance.set(tolerance);
        if percent(tolerance) != was_percent {
            self.toaster
                .info(&format!("Threshold {}%", percent(tolerance)));
        }
        fill_request(&self.fill_ctx());
    }

    /// Lift: the threshold the user settled on is the fill, so record it
    /// as one history entry. A fill still computing records itself when
    /// it lands instead.
    fn fill_end(&self) {
        let done = {
            let mut guard = self.fill_session.borrow_mut();
            let Some(s) = guard.as_mut() else {
                return;
            };
            s.released = true;
            !s.in_flight && !s.restart
        };
        // Catch the slider up now the gesture is over.
        self.fill.set_tolerance(self.fill.tolerance.get());
        if done {
            fill_finish(&self.fill_ctx());
        }
    }

    /// The pieces the fill's async plumbing needs, bundled so the poll
    /// timers can carry them without reaching back into the handler.
    fn fill_ctx(&self) -> FillCtx {
        FillCtx {
            canvas: Rc::clone(&self.canvas),
            paintable: self.paintable.clone(),
            area: self.area.clone(),
            tools: self.tools.clone(),
            history: Rc::clone(&self.history),
            session: Rc::clone(&self.fill_session),
            reveal: Rc::clone(&self.fill_reveal),
            pump: self.render_pump.clone(),
        }
    }

    /// Record whatever a fill gesture has already put on the layer.
    /// No-op when no fill is in progress.
    fn finalize_pending_fill(&self) {
        fill_finish(&self.fill_ctx());
    }

    /// Cut any running fill reveal short, leaving the committed pixels
    /// fully visible. Undo/redo go through this so an animation can't
    /// keep masking a fill that is no longer on the canvas.
    fn end_fill_animation(&self) {
        fill_reveal_stop(&self.fill_ctx(), true);
    }

    // -- shapes ------------------------------------------------------------

    fn shape_begin(&self, _kind: ShapeTool, x: f64, y: f64) {
        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation);
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
        let cur = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom, &self.rotation);
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

    // -- gradient ----------------------------------------------------------

    fn gradient_begin(&self, x: f64, y: f64) {
        let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation);
        self.gradient_drag_start.set(canvas_pos);
        self.gradient_cur_endpoints.set(None);

        // Capture the target layer's pristine pixels for the undo patch. The
        // selection clip is sampled directly by the shader from the GPU mask.
        let pending = {
            let mut canvas = self.canvas.borrow_mut();
            let Some(idx) = canvas.layers().active() else {
                *self.gradient_pending.borrow_mut() = None;
                return;
            };
            let id = canvas.layers().snapshot()
                .get(idx).map(|l| l.id.clone()).unwrap_or_default();
            match canvas.read_layer(idx) {
                Ok(before) => Some(GradientPending { idx, id, before }),
                Err(e) => {
                    tracing::error!(error = %e, "gradient: read_layer failed");
                    None
                }
            }
        };
        *self.gradient_pending.borrow_mut() = pending;

        // Arm the GPU overlay and upload the baked ramp LUT once.
        if let Some(p) = self.gradient_pending.borrow().as_ref() {
            let lut = self.gradient.resolve(&self.colors).bake_lut();
            let mut canvas = self.canvas.borrow_mut();
            canvas.begin_gradient_overlay(p.idx);
            if let Err(e) = canvas.set_gradient_lut(&lut) {
                tracing::error!(error = %e, "gradient: set_gradient_lut failed");
            }
        }
    }

    fn gradient_update(&self, gesture: &gtk::GestureDrag, dx: f64, dy: f64) {
        let Some((sx, sy)) = gesture.start_point() else {
            return;
        };
        let cur = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom, &self.rotation);
        let start = self.gradient_drag_start.get();
        let endpoints = [start.x, start.y, cur.x, cur.y];
        self.gradient_cur_endpoints.set(Some(endpoints));

        let kind = self.gradient.gradient_type.get().to_renderer_kind();
        let mut canvas = self.canvas.borrow_mut();
        canvas.set_gradient_preview_params(kind, endpoints);
        present_into_paintable(&mut canvas, &self.paintable, &self.area);
    }

    fn gradient_end(&self) {
        let Some(endpoints) = self.gradient_cur_endpoints.take() else {
            self.canvas.borrow_mut().cancel_gradient_overlay();
            self.gradient_pending.borrow_mut().take();
            present_into_paintable(&mut self.canvas.borrow_mut(), &self.paintable, &self.area);
            return;
        };
        let Some(pending) = self.gradient_pending.borrow_mut().take() else {
            self.canvas.borrow_mut().cancel_gradient_overlay();
            return;
        };

        // Ignore a click / zero-length drag.
        let dx = endpoints[2] - endpoints[0];
        let dy = endpoints[3] - endpoints[1];
        if dx * dx + dy * dy < 1.0 {
            self.canvas.borrow_mut().cancel_gradient_overlay();
            present_into_paintable(&mut self.canvas.borrow_mut(), &self.paintable, &self.area);
            return;
        }

        let kind = self.gradient.gradient_type.get().to_renderer_kind();
        let cs = {
            let mut canvas = self.canvas.borrow_mut();
            if let Err(e) = canvas.commit_gradient(pending.idx, kind, endpoints) {
                tracing::error!(error = %e, "gradient: commit_gradient failed");
                canvas.cancel_gradient_overlay();
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
                tracing::error!(error = %e, "gradient: read_layer after commit failed");
                present_into_paintable(&mut self.canvas.borrow_mut(), &self.paintable, &self.area);
                return;
            }
        };
        if let Some(patch) =
            LayerPatch::from_full_diff(&pending.before, &after, cs.width, cs.height)
        {
            self.history.borrow_mut().record(HistoryAction::Gradient {
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
            let canvas_pos = widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation);
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
        let start = widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation);
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
        let cur = widget_to_canvas(sx + dx, sy + dy, &self.pan, &self.zoom, &self.rotation);
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

/// Round a canvas-space point to the nearest whole-pixel grid line so a
/// rectangular marquee always covers complete pixels.
fn snap_to_pixel(p: Point) -> Point {
    Point::new(p.x.round(), p.y.round())
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
    // iso=1 so the outline hugs every non-empty pixel, including the soft
    // anti-aliased edge of an alpha-derived selection.
    let contours = oxiedraw_core::selection::pixel_perfect_contours(&mask, mw, mh, 1);
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
/// Which kind of device drove the gesture's current event. Only used to
/// tell pen input from mouse input in debug logs.
fn event_source(gesture: &gtk::GestureDrag) -> Option<gtk::gdk::InputSource> {
    gesture
        .current_event()
        .and_then(|e| e.device())
        .map(|d| d.source())
}

/// How far sideways the pointer travels to sweep the whole threshold
/// range. Matches the feel of dragging out a ColorDrop.
const TOLERANCE_DRAG_RANGE_PX: f64 = 400.0;

/// Sideways slack before a fill press counts as a threshold drag, so a
/// slightly shaky tap is still just a tap.
const TOLERANCE_DRAG_DEAD_ZONE_PX: f64 = 8.0;

/// How long the reveal sweep takes end to end.
const REVEAL_MS: u64 = 400;

/// How far the sweep has got at `elapsed_ms`, or `None` once it is done.
///
/// Ease-out cubic: fast at first, slows at the edges - feels like
/// ink/paint spreading and dragging at its boundary. The mask stores
/// 0..=254 across the fill region, so the radius is capped just under
/// the sentinel value for every in-region pixel to be admitted at the end.
fn reveal_at(elapsed_ms: u64) -> Option<f32> {
    if elapsed_ms >= REVEAL_MS {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let t = (elapsed_ms as f32 / REVEAL_MS as f32).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - t).powi(3);
    let max_reveal = 254.0 / 255.0;
    Some((eased * max_reveal).clamp(0.0, max_reveal))
}

/// The threshold as the percentage the user sees.
const fn percent(tolerance: u8) -> u32 {
    tolerance as u32 * 100 / 255
}

/// A fill gesture, live from press to release.
///
/// The buffers are shared with the worker thread rather than moved, so a
/// threshold adjustment can re-run the flood fill without re-reading the
/// canvas. `before` is the layer as it was at press: every recomputed
/// fill is painted from it, so dragging back and forth never compounds.
struct FillSession {
    /// Identifies this gesture, so a flood fill that finishes after its
    /// own session ended can be dropped instead of being applied to
    /// whatever gesture is running by then.
    generation: u64,
    layer_idx: usize,
    layer_id: String,
    before: Arc<Vec<u8>>,
    sample: Option<Arc<Vec<u8>>>,
    occluders: Option<Arc<Vec<u8>>>,
    mask: Option<Arc<Vec<u8>>>,
    seed: (i32, i32),
    size: (u32, u32),
    color_bgr: [u8; 3],
    auto_edge: bool,
    /// Widget x at press, so a plain pointer position is enough to work
    /// out how far the threshold has been dragged.
    origin_x: f64,
    /// Threshold at press; the drag offset is measured from here.
    base_tolerance: u8,
    tolerance: u8,
    /// Pixels currently on the layer, kept for the history diff.
    current: Option<Vec<u8>>,
    in_flight: bool,
    /// The threshold moved on while a flood fill was already running, so
    /// one more pass is owed once it lands. Intermediate positions are
    /// never worth computing - only where the pointer ended up.
    restart: bool,
    released: bool,
    animate: bool,
}

/// The pieces the fill's async plumbing needs.
#[derive(Clone)]
struct FillCtx {
    canvas: Rc<RefCell<Canvas>>,
    paintable: CanvasPaintable,
    area: gtk::Picture,
    tools: ToolState,
    history: Rc<RefCell<HistoryStack>>,
    session: Rc<RefCell<Option<FillSession>>>,
    /// When the reveal sweep started, if one is running. It outlives the
    /// session on purpose: a gesture that ends mid-sweep must not strand
    /// the overlay hiding a fill that is already committed.
    reveal: Rc<Cell<Option<std::time::Instant>>>,
    pump: RenderPump,
}

/// Run the flood fill for the session's current threshold on a worker
/// thread, then poll for it.
///
/// The BFS is off the main thread because at 8k canvas it is the
/// difference between a 1.5s freeze and a smooth-but-pending click. Only
/// one runs at a time; anything asked for meanwhile is coalesced into
/// `queued` and picked up when this one lands.
fn fill_request(ctx: &FillCtx) {
    let Some(job) = ({
        let mut guard = ctx.session.borrow_mut();
        let Some(s) = guard.as_mut() else {
            return;
        };
        if s.in_flight {
            // Let the running pass finish and re-run for wherever the
            // pointer has got to by then.
            s.restart = true;
            return;
        }
        s.in_flight = true;
        s.restart = false;
        let generation = s.generation;
        Some(FillJob {
            generation,
            before: Arc::clone(&s.before),
            sample: s.sample.clone(),
            occluders: s.occluders.clone(),
            mask: s.mask.clone(),
            seed: s.seed,
            size: s.size,
            opts: FillOptions {
                tolerance: s.tolerance,
                auto_edge: s.auto_edge,
                // Only the first showing is ever swept in; every
                // threshold step after it appears at once, so it can
                // skip the distance sort and mask entirely.
                reveal_order: s.animate,
            },
        })
    }) else {
        return;
    };

    let (tx, rx) = std::sync::mpsc::channel::<Option<FillResult>>();
    let generation = job.generation;
    std::thread::spawn(move || {
        // Seed/match against the composite when sampling all layers;
        // the paint still lands in the active layer, which is what
        // decides how the fill combines with what's already there.
        let source = FillSource {
            sample: job.sample.as_ref().map_or(&job.before[..], |s| &s[..]),
            target: &job.before,
            occluders: job.occluders.as_ref().map(|o| &o[..]),
            mask: job.mask.as_ref().map(|m| &m[..]),
        };
        let (w, h) = job.size;
        let (sx, sy) = job.seed;
        let started = std::time::Instant::now();
        let result = flood_fill(&source, w, h, sx, sy, job.opts);
        tracing::debug!(
            ms = started.elapsed().as_millis(),
            tolerance = job.opts.tolerance,
            "fill: flood fill computed",
        );
        let _ = tx.send(result);
    });

    // The poll is deliberately not tracked alongside the animation: a
    // fill in flight has to be allowed to land even if the user clicks
    // again straight away.
    let ctx = ctx.clone();
    glib::timeout_add_local(
        std::time::Duration::from_millis(16),
        move || match rx.try_recv() {
            Ok(result) => {
                fill_arrived(&ctx, result, generation);
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        },
    );
}

/// Everything the worker thread needs, detached from the session.
struct FillJob {
    generation: u64,
    before: Arc<Vec<u8>>,
    sample: Option<Arc<Vec<u8>>>,
    occluders: Option<Arc<Vec<u8>>>,
    mask: Option<Arc<Vec<u8>>>,
    seed: (i32, i32),
    size: (u32, u32),
    opts: FillOptions,
}

/// A flood fill came back: put it on the layer, then either sweep it in,
/// run whatever threshold was asked for in the meantime, or - if the
/// pointer is already up - close the gesture out.
fn fill_arrived(ctx: &FillCtx, result: Option<FillResult>, generation: u64) {
    // A flood fill that outlived the gesture that asked for it belongs
    // to nothing: applying it to whatever gesture is running now would
    // paint the wrong region and, worse, hand its `in_flight` flag back
    // so two workers could run at once.
    if ctx
        .session
        .borrow()
        .as_ref()
        .is_none_or(|s| s.generation != generation)
    {
        return;
    }
    // Switching tools mid-flight means the user is no longer expecting
    // this fill to land. Close the session out rather than dropping it -
    // an earlier threshold may already be on the layer, and it still
    // belongs in the undo stack.
    if !matches!(ctx.tools.active.get(), Tool::Fill(FillTool::Bucket)) {
        fill_finish(ctx);
        return;
    }

    let started = std::time::Instant::now();
    let mut painted = None;
    let mut animate = false;
    let mut changed = false;
    {
        let mut guard = ctx.session.borrow_mut();
        let Some(s) = guard.as_mut() else {
            return;
        };
        s.in_flight = false;
        let matched = result.filter(|r| !r.sorted_indices.is_empty());
        if matched.is_some() || s.current.is_some() {
            // Always paint from the pristine copy: a threshold drag
            // replaces the previous fill rather than stacking on it. A
            // threshold that now matches nothing lands back on the
            // untouched layer, so the canvas tracks the drag either way.
            let mut buf = s.current.take().unwrap_or_else(|| (*s.before).clone());
            buf.copy_from_slice(&s.before);
            if let Some(result) = &matched {
                paint_fill(&mut buf, result, s.color_bgr);
            }
            s.current = Some(buf);
            animate = s.animate && matched.is_some();
            s.animate = false;
            painted = matched;
            changed = true;
        }
    }

    if changed {
        fill_paint_layer(ctx, painted.as_ref(), animate);
        tracing::debug!(
            ms = started.elapsed().as_millis(),
            "fill: painted and presented",
        );
    }

    let (restart, released) = {
        let mut guard = ctx.session.borrow_mut();
        let Some(s) = guard.as_mut() else {
            return;
        };
        (std::mem::take(&mut s.restart), s.released)
    };
    if restart {
        fill_request(ctx);
    } else if released {
        fill_finish(ctx);
    }
}

/// Push the session's current pixels onto the layer, and arm the reveal
/// sweep when this is the fill's first showing.
fn fill_paint_layer(ctx: &FillCtx, result: Option<&FillResult>, animate: bool) {
    tracing::debug!(animate, "fill: writing pixels to layer");
    let mut c = ctx.canvas.borrow_mut();
    let mut guard = ctx.session.borrow_mut();
    let Some(s) = guard.as_mut() else {
        return;
    };
    // Layers can be reordered or removed while the flood fill is
    // computing, so the id is what identifies the target - never the
    // index it happened to have at press. If the layer is gone the fill
    // is abandoned: writing a whole-layer buffer into whatever now
    // occupies that slot would destroy it, and the history entry is
    // keyed on the missing id so undo couldn't put it back.
    let layers = c.layers().snapshot();
    let Some(layer_idx) = layers.iter().position(|l| l.id == s.layer_id) else {
        tracing::warn!(layer = %s.layer_id, "fill: target layer is gone, abandoning");
        drop(layers);
        drop(guard);
        ctx.session.borrow_mut().take();
        return;
    };
    drop(layers);
    s.layer_idx = layer_idx;
    let Some(buf) = s.current.as_ref() else {
        return;
    };
    if let Err(e) = c.commit_fill(layer_idx, buf) {
        tracing::error!(error = %e, "fill: commit_fill failed");
        return;
    }
    // Arm the reveal: one mask upload + state setup, which leaves the
    // fill hidden at radius zero. If it fails the fill simply appears at
    // once, which is no worse than the sweep.
    let armed = animate
        && result.is_some_and(|r| match c.begin_fill_overlay(layer_idx, r) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(error = %e, "fill: begin_fill_overlay failed");
                false
            }
        });
    present_into_paintable(&mut c, &ctx.paintable, &ctx.area);
    drop(guard);
    drop(c);
    tracing::debug!(armed, "fill: pixels on screen");
    if armed {
        // Hand the sweep to the frame clock. It runs at vsync whether or
        // not the pointer moves, and outlives both this gesture and the
        // drag that armed the pump.
        ctx.reveal.set(Some(std::time::Instant::now()));
        ctx.pump.arm();
    }
}

/// Advance the reveal sweep by one frame. Called from the render pump's
/// frame-clock tick, which presents straight afterwards - so this only
/// moves canvas state and never presents itself.
fn fill_reveal_tick(ctx: &FillCtx) {
    let Some(started) = ctx.reveal.get() else {
        return;
    };
    #[allow(clippy::cast_possible_truncation)]
    let elapsed = started.elapsed().as_millis() as u64;
    let mut c = ctx.canvas.borrow_mut();
    // Undo, or a fresh fill, may have taken the overlay down already.
    if !c.fill_overlay_active() {
        drop(c);
        fill_reveal_stop(ctx, false);
        return;
    }
    if let Some(reveal) = reveal_at(elapsed) {
        c.set_fill_reveal(reveal);
    } else {
        c.cancel_fill_overlay();
        drop(c);
        fill_reveal_stop(ctx, false);
    }
}

/// End the sweep, releasing the pump reference exactly once. `show`
/// drops the overlay so the finished fill is visible - used when
/// something interrupts the sweep rather than letting it complete.
fn fill_reveal_stop(ctx: &FillCtx, show: bool) {
    if ctx.reveal.take().is_none() {
        return;
    }
    ctx.pump.disarm();
    if show {
        let mut c = ctx.canvas.borrow_mut();
        if c.fill_overlay_active() {
            c.cancel_fill_overlay();
            present_into_paintable(&mut c, &ctx.paintable, &ctx.area);
        }
    }
}

/// Close the gesture out: one history entry for the threshold the user
/// settled on, whatever they tried on the way there.
fn fill_finish(ctx: &FillCtx) {
    let Some(s) = ctx.session.borrow_mut().take() else {
        return;
    };
    let Some(after) = s.current else {
        return;
    };
    let cs = ctx.canvas.borrow().size();
    if let Some(patch) = LayerPatch::from_full_diff(&s.before, &after, cs.width, cs.height) {
        ctx.history.borrow_mut().record(HistoryAction::Fill {
            layer_id: s.layer_id,
            patch,
        });
    }
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
    gradient: &GradientState,
    guide: &GuideState,
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
        rotation: Rc::clone(&viewport.rotation),
        canvas_size: Rc::clone(&viewport.canvas_size),
        tools: tools.clone(),
        area: area.clone(),
        toaster: toaster.clone(),
        brush_engine: brush_engine.clone(),
        colors: colors.clone(),
        stroke_points: Rc::new(RefCell::new(Vec::new())),
        pending_color: Rc::new(Cell::new(Color::new(0, 0, 0))),
        pending_opacity: Rc::new(Cell::new(1.0)),
        pending_erase: Rc::new(Cell::new(false)),
        pending_timer: Rc::new(RefCell::new(None)),
        pending_correction: Rc::new(RefCell::new(None)),
        stroke_shape_correction: RefCell::new(ShapeCorrectionSettings::default()),
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
        fill_reveal: Rc::new(Cell::new(None)),
        fill_generation: Rc::new(Cell::new(0)),
        render_pump: viewport.render_pump(),
        fill_session: Rc::new(RefCell::new(None)),
        shape: shape.clone(),
        shape_drag_start: Rc::new(Cell::new(Point::ZERO)),
        shape_cur_rect: Rc::new(Cell::new(None)),
        shape_pending: Rc::new(RefCell::new(None)),
        gradient: gradient.clone(),
        gradient_drag_start: Rc::new(Cell::new(Point::ZERO)),
        gradient_cur_endpoints: Rc::new(Cell::new(None)),
        gradient_pending: Rc::new(RefCell::new(None)),
        guide: guide.clone(),
        guide_drag: Rc::new(Cell::new(GuideDrag::None)),
        guide_drag_start: Rc::new(Cell::new(None)),
        guide_vp_drag: Rc::new(Cell::new(None)),
        guide_pending_add: Rc::new(Cell::new(None)),
        guide_snap_start: Rc::new(Cell::new(None)),
        guide_snap: Rc::new(RefCell::new(None)),
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

    // Keep the frame clock at steady vsync for the whole drag, so a jittery
    // stylus event stream still renders smoothly. See `RenderPump`.
    {
        let h = Rc::clone(&handler);
        let pump = viewport.render_pump();
        drag.connect_drag_begin(move |g, x, y| {
            pump.arm();
            h.on_begin(g, x, y);
        });
    }
    {
        let h = Rc::clone(&handler);
        drag.connect_drag_update(move |g, dx, dy| h.on_update(g, dx, dy));
    }
    {
        let h = Rc::clone(&handler);
        let pump = viewport.render_pump();
        drag.connect_drag_end(move |g, _, _| {
            h.on_end(g);
            pump.disarm();
        });
    }

    // The fill's reveal sweep advances on the render pump's frame clock:
    // vsync-paced whether or not the pointer moves, and in phase with
    // the present that is already happening rather than blocking the
    // main loop between input events.
    {
        let ctx = handler.fill_ctx();
        *viewport.render_pump().tick_handle().borrow_mut() =
            Some(Box::new(move || fill_reveal_tick(&ctx)));
    }

    // The motion controller feeds the fill's threshold drag too - see
    // the note there for why the drag gesture alone isn't enough.
    {
        let h = Rc::clone(&handler);
        *viewport.fill_drag_handle().borrow_mut() =
            Some(Box::new(move |x, held| h.fill_update_at(x, held)));
    }

    // Let undo/redo land an in-flight shape correction before they touch
    // history, so the corrected shape is recorded rather than silently
    // overwriting an older action's undo state. A running fill reveal is
    // cut short at the same point - its pixels are already committed, so
    // leaving it running would mask a fill that undo just removed.
    {
        let h = Rc::clone(&handler);
        *viewport.flush_correction_handle().borrow_mut() = Some(Box::new(move || {
            h.finalize_pending_brush();
            h.finalize_pending_fill();
            h.end_fill_animation();
        }));
    }

    area.add_controller(drag);
}
