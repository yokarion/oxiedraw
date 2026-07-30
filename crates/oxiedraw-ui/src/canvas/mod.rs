//! Canvas viewport, gesture wiring, and present path.
//!
//! `Viewport` owns the headless `Canvas` and the GTK paintable that
//! displays its dmabuf output. `wire()` attaches input controllers to a
//! `gtk::Picture` once the realized widget exists.

mod crop_geom;
pub(crate) mod primary_drag;
mod transform_geometry;

use std::cell::{Cell, RefCell};
use std::os::fd::AsRawFd;
use std::rc::Rc;

use oxiedraw_core::brush_engine::{
    BrushEngine, InputSample, StrokeContext, compute_brush_cursor, make_spawn_input,
};
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::ColorState;
use oxiedraw_core::document::LayerState;
use oxiedraw_core::renderer::DmabufDescriptor;
use oxiedraw_core::guides::GuideState;
use oxiedraw_core::tools::{
    CropRect, CropState, FillState, FillTool, GradientState, SelectionState, ShapeState, Tool,
    ToolState, TransformState,
};
use oxiedraw_utils::geometry::{Point, Size};

use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use oxiedraw_core::color::Color;

use crate::canvas_paintable::{CanvasPaintable, ColorPickerOverlay, GradientCursorOverlay};

pub(super) const BUTTON_PRIMARY: u32 = 1;
const BUTTON_MIDDLE: u32 = 2;
const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 32.0;
const ZOOM_STEP: f64 = 1.1;
const DEFAULT_FIT_RATIO: f64 = 0.5;
/// Widget pixels of vertical drag that change the zoom by one octave
/// (factor of 2) during a Ctrl+middle-drag zoom, Krita-style.
const ZOOM_DRAG_OCTAVE_PX: f64 = 150.0;
/// Multiplier applied to touchpad two-finger scroll deltas (surface unit,
/// already pixel-scaled) when panning the canvas.
const TOUCHPAD_PAN_SPEED: f32 = 1.0;
/// Exponential-smoothing time constant (seconds) for the snap ease. Time-based
/// so the animation is frame-rate independent. ~3x this = settle time.
const SNAP_EASE_TAU: f64 = 0.012;
/// Stop the snap ease once within this many radians of the target.
const SNAP_EASE_EPSILON: f32 = 0.0015;

/// Rotation snap step in radians, from settings (degrees). Falls back to 45 deg.
pub(crate) fn rotation_snap_rad() -> f32 {
    let deg = crate::settings::AppSettings::load().rotation_snap_deg;
    deg.to_radians().max(0.017) // >= ~1 deg to avoid divide-by-zero snapping
}


/// Which navigation gesture (if any) the middle-mouse drag is currently
/// performing. Drives both the gesture handlers and the cursor that the
/// motion handler must leave alone while a nav drag is in progress.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NavDrag {
    None,
    Pan,
    Zoom,
    Rotate,
}

/// Late-bound "redraw the canvas" callable. Created empty by
/// `Viewport::new` and populated by `wire()` once the GTK `Picture`
/// widget exists. Any code that mutates the `Canvas` (brush events,
/// layer-panel handlers, ...) calls [`RedrawHandle::request`] afterward
/// so the UI re-presents and re-textures.
type RedrawFn = Rc<dyn Fn()>;

#[derive(Clone, Default)]
pub(crate) struct RedrawHandle(Rc<RefCell<Option<RedrawFn>>>);

impl RedrawHandle {
    fn set(&self, cb: RedrawFn) {
        *self.0.borrow_mut() = Some(cb);
    }
    pub(crate) fn request(&self) {
        let cb = self.0.borrow().clone();
        if let Some(cb) = cb {
            cb();
        }
    }
}

/// View state + the headless Vulkan canvas + the GTK paintable that
/// displays its dmabuf output.
///
/// All members are reference-counted so the GTK event closures can
/// clone the handles they need without `&mut`-ing a top-level struct.
/// `canvas` is wrapped in `RefCell` because every Vulkan op is `&mut`;
/// GTK4 is single-threaded so the borrows are always serialised in
/// practice.
#[derive(Clone)]
pub(crate) struct Viewport {
    pan: Rc<Cell<Point>>,
    /// Cumulative `(dx, dy)` reported by the active middle-mouse pan drag at
    /// the previous update. Reset on drag-begin so each drag-update applies
    /// only the incremental difference - this keeps the pan stable when
    /// something else (e.g. a scroll-wheel zoom firing concurrently from
    /// touchpad kinetic scrolling) modifies `pan` mid-drag.
    pan_last_offset: Rc<Cell<Point>>,
    zoom: Rc<Cell<f32>>,
    /// Canvas rotation in radians (view-only; also persisted per document).
    rotation: Rc<Cell<f32>>,
    /// Accumulated free (un-snapped) rotation target during a rotate drag.
    /// Seeded with the current rotation at drag-begin, then advanced by the
    /// pointer's angular travel around the pivot each update (Krita-style).
    nav_rotate_target: Rc<Cell<f32>>,
    /// Pointer angle (radians, around the viewport centre) at the previous
    /// rotate update, for accumulating angular travel across the branch cut.
    nav_rotate_last_angle: Rc<Cell<f32>>,
    /// Snap modifier mask + step (radians) resolved once at rotate-drag begin,
    /// so the per-event update path never re-reads settings from disk.
    nav_rotate_snap_mask: Rc<Cell<Option<gdk::ModifierType>>>,
    nav_rotate_snap_step: Rc<Cell<f32>>,
    /// Target angle (radians) the snap ease is animating toward, or `None` when
    /// idle. Set while rotating with the snap modifier held.
    snap_target: Rc<Cell<Option<f32>>>,
    /// Whether the snap-ease frame tick is currently installed.
    snap_tick_installed: Rc<Cell<bool>>,
    /// Frame-clock time (us) of the previous ease tick, for the time-based lerp.
    snap_last_time: Rc<Cell<i64>>,
    /// Observer fired on every view change (pan/zoom/rotation) so the
    /// per-canvas info bar can refresh its size + angle readout. Installed by
    /// the session once the info bar widget exists.
    info_observer: Rc<RefCell<Option<Box<dyn Fn(Size, f32)>>>>,
    cursor: Rc<Cell<Point>>,
    /// Active middle-mouse navigation gesture (pan vs. Ctrl+drag zoom),
    /// or `None` when idle. Set on drag-begin, cleared on drag-end; the
    /// motion handler reads it so it won't fight the grab / zoom cursor.
    nav: Rc<Cell<NavDrag>>,
    /// Zoom factor captured at the start of a Ctrl+middle zoom drag, so
    /// the cumulative drag offset maps to an absolute zoom level.
    nav_zoom_start: Rc<Cell<f32>>,
    /// Widget-space point the zoom drag pivots around (the drag origin).
    nav_anchor: Rc<Cell<Point>>,
    /// Zoom factor captured when a touchpad pinch-zoom gesture begins, so the
    /// gesture's cumulative scale maps to an absolute zoom level.
    pinch_zoom_start: Rc<Cell<f32>>,
    centered: Rc<Cell<bool>>,
    canvas: Rc<RefCell<Canvas>>,
    paintable: CanvasPaintable,
    redraw: RedrawHandle,
    canvas_size: Rc<Cell<Size>>,
    picture: Rc<RefCell<Option<gtk::Picture>>>,
    /// Set by the brush handler while a shape-correction stroke is mid-flight
    /// (idle timer armed or animation playing). undo/redo invoke it first so
    /// the in-flight stroke is committed and recorded before history mutates,
    /// keeping the canvas and the undo stack in sync.
    flush_correction: Rc<RefCell<Option<Box<dyn Fn()>>>>,
    render_pump: RenderPump,
}

/// Keeps GTK's frame clock continuously updating during an interactive drag, so
/// the canvas renders at steady vsync regardless of input-event timing.
///
/// We only schedule a redraw when an input event arrives. A high-rate mouse
/// (~160 Hz) delivers an event almost every frame, which keeps the frame clock
/// "updating" and paced at vsync. A stylus delivers motion in jittery coalesced
/// bursts, so between bursts the clock has nothing to do and stops; the next
/// event restarts it, re-syncing with the compositor and costing a frame or two
/// - visible as jitter on the pen but not the mouse. While armed, a frame-clock
/// tick re-presents every frame, keeping the clock hot (what a continuous
/// render loop does). It self-removes when no drag is active, so idle is free.
#[derive(Clone)]
pub(crate) struct RenderPump {
    canvas: Rc<RefCell<Canvas>>,
    paintable: CanvasPaintable,
    picture: Rc<RefCell<Option<gtk::Picture>>>,
    active: Rc<Cell<u32>>,
    installed: Rc<Cell<bool>>,
}

impl RenderPump {
    fn new(
        canvas: Rc<RefCell<Canvas>>,
        paintable: CanvasPaintable,
        picture: Rc<RefCell<Option<gtk::Picture>>>,
    ) -> Self {
        Self {
            canvas,
            paintable,
            picture,
            active: Rc::new(Cell::new(0)),
            installed: Rc::new(Cell::new(false)),
        }
    }

    /// Begin (or join) an interaction: ensure the per-frame present tick runs.
    pub(crate) fn arm(&self) {
        self.active.set(self.active.get() + 1);
        if self.installed.replace(true) {
            return;
        }
        let Some(area) = self.picture.borrow().clone() else {
            self.installed.set(false);
            return;
        };
        let me = self.clone();
        area.add_tick_callback(move |area, _clock| {
            if me.active.get() == 0 {
                me.installed.set(false);
                return gtk::glib::ControlFlow::Break;
            }
            let changed = me.canvas.borrow().present_would_redraw();
            if changed {
                present_into_paintable(&mut me.canvas.borrow_mut(), &me.paintable, area);
            } else {
                // Canvas pixels are unchanged - a pan/zoom that only moves the view,
                // or a gap between stylus event bursts. Skip the costly dmabuf
                // re-import, but still repaint so the frame clock stays hot and the
                // view re-composites at the current transform every frame. Without
                // this, a pan redraws only on input events, so a stylus's bursty
                // delivery makes panning visibly jitter while a mouse stays smooth.
                area.queue_draw();
            }
            gtk::glib::ControlFlow::Continue
        });
    }

    /// End one interaction; the tick stops once the last one ends.
    pub(crate) fn disarm(&self) {
        self.active.set(self.active.get().saturating_sub(1));
    }
}

impl std::fmt::Debug for Viewport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Viewport")
            .field("pan", &self.pan.get())
            .field("zoom", &self.zoom.get())
            .field("rotation", &self.rotation.get())
            .finish_non_exhaustive()
    }
}

/// Closure-friendly bundle of the view-transform cells + paintable + info-bar
/// observer. Gesture handlers clone one of these and call [`ViewSync::commit`]
/// after mutating the cells, so the paintable transform and the bottom info bar
/// stay in lockstep through a single path.
#[derive(Clone)]
struct ViewSync {
    pan: Rc<Cell<Point>>,
    zoom: Rc<Cell<f32>>,
    rotation: Rc<Cell<f32>>,
    canvas_size: Rc<Cell<Size>>,
    paintable: CanvasPaintable,
    info_observer: Rc<RefCell<Option<Box<dyn Fn(Size, f32)>>>>,
}

impl ViewSync {
    /// Push the current pan/zoom/rotation to the paintable and notify the info
    /// bar. Call after mutating any of the cells.
    fn commit(&self) {
        let p = self.pan.get();
        self.paintable
            .set_transform(p.x, p.y, self.zoom.get(), self.rotation.get());
        if let Some(cb) = self.info_observer.borrow().as_ref() {
            cb(self.canvas_size.get(), self.rotation.get());
        }
    }
}

/// Eases the view rotation toward a target angle over a few frames, pivoting
/// around the viewport centre. Used only while rotating with the snap modifier
/// held, so the canvas glides to each 45 deg stop instead of jumping.
#[derive(Clone)]
struct RotationAnimator {
    sync: ViewSync,
    picture: Rc<RefCell<Option<gtk::Picture>>>,
    target: Rc<Cell<Option<f32>>>,
    installed: Rc<Cell<bool>>,
    last_time: Rc<Cell<i64>>,
}

impl RotationAnimator {
    /// (Re)aim the ease at `target` and ensure the frame tick is running.
    fn animate_to(&self, target: f32) {
        self.target.set(Some(target));
        if self.installed.replace(true) {
            return; // tick already running; it will pick up the new target
        }
        let Some(area) = self.picture.borrow().clone() else {
            self.installed.set(false);
            return;
        };
        self.last_time.set(0);
        let me = self.clone();
        area.add_tick_callback(move |_area, clock| {
            let Some(target) = me.target.get() else {
                me.installed.set(false);
                return glib::ControlFlow::Break;
            };
            let cur = me.sync.rotation.get();
            let delta = target - cur;
            if delta.abs() < SNAP_EASE_EPSILON {
                me.target.set(None);
                me.installed.set(false);
                me.apply(target);
                return glib::ControlFlow::Break;
            }
            // Frame-rate-independent exponential smoothing.
            let now = clock.frame_time();
            let last = me.last_time.replace(now);
            let dt = if last == 0 {
                1.0 / 60.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let d = (now - last) as f64 / 1_000_000.0;
                d.clamp(0.0, 0.1)
            };
            #[allow(clippy::cast_possible_truncation)]
            let factor = (1.0 - (-dt / SNAP_EASE_TAU).exp()) as f32;
            me.apply(delta.mul_add(factor, cur));
            glib::ControlFlow::Continue
        });
    }

    /// Stop the ease where it is (used when snap is released mid-drag).
    fn cancel(&self) {
        self.target.set(None);
    }

    fn apply(&self, theta: f32) {
        rotate_about_center(
            &self.sync.pan,
            &self.sync.zoom,
            &self.sync.rotation,
            &self.picture,
            theta,
        );
        self.sync.commit();
        if let Some(area) = self.picture.borrow().as_ref() {
            area.queue_draw();
        }
    }
}

impl Viewport {
    pub(crate) fn new(canvas_size: Size, layers: LayerState) -> Self {
        let canvas = Canvas::new(canvas_size, layers).expect("Vulkan canvas init");
        let paintable = CanvasPaintable::new(canvas_size.width, canvas_size.height);
        let canvas = Rc::new(RefCell::new(canvas));
        let picture = Rc::new(RefCell::new(None));
        let render_pump = RenderPump::new(Rc::clone(&canvas), paintable.clone(), Rc::clone(&picture));
        Self {
            pan: Rc::new(Cell::new(Point::ZERO)),
            pan_last_offset: Rc::new(Cell::new(Point::ZERO)),
            zoom: Rc::new(Cell::new(1.0)),
            rotation: Rc::new(Cell::new(0.0)),
            nav_rotate_target: Rc::new(Cell::new(0.0)),
            nav_rotate_last_angle: Rc::new(Cell::new(0.0)),
            nav_rotate_snap_mask: Rc::new(Cell::new(None)),
            nav_rotate_snap_step: Rc::new(Cell::new(0.0)),
            snap_target: Rc::new(Cell::new(None)),
            snap_tick_installed: Rc::new(Cell::new(false)),
            snap_last_time: Rc::new(Cell::new(0)),
            info_observer: Rc::new(RefCell::new(None)),
            cursor: Rc::new(Cell::new(Point::ZERO)),
            nav: Rc::new(Cell::new(NavDrag::None)),
            nav_zoom_start: Rc::new(Cell::new(1.0)),
            nav_anchor: Rc::new(Cell::new(Point::ZERO)),
            pinch_zoom_start: Rc::new(Cell::new(1.0)),
            centered: Rc::new(Cell::new(false)),
            canvas,
            paintable,
            redraw: RedrawHandle::default(),
            canvas_size: Rc::new(Cell::new(canvas_size)),
            picture,
            flush_correction: Rc::new(RefCell::new(None)),
            render_pump,
        }
    }

    /// Handle to the render pump - armed/disarmed around interactive drags to
    /// keep the frame clock at steady vsync. See [`RenderPump`].
    pub(crate) fn render_pump(&self) -> RenderPump {
        self.render_pump.clone()
    }

    /// Cloneable slot the brush handler installs its "finalize the in-flight
    /// shape-correction stroke" callback into. Lives on the viewport so the
    /// app-level undo/redo handlers can drive it without reaching into the
    /// gesture internals.
    pub(crate) fn flush_correction_handle(&self) -> Rc<RefCell<Option<Box<dyn Fn()>>>> {
        Rc::clone(&self.flush_correction)
    }

    /// Commit + record any in-flight shape-correction stroke. No-op when
    /// nothing is pending. Called by undo/redo before they touch history so a
    /// half-corrected stroke can't desync the canvas from the undo stack.
    pub(crate) fn flush_pending_correction(&self) {
        if let Some(cb) = self.flush_correction.borrow().as_ref() {
            cb();
        }
    }

    pub(crate) const fn paintable(&self) -> &CanvasPaintable {
        &self.paintable
    }

    /// Toggle the frame-time performance overlay on this document's canvas.
    pub(crate) fn toggle_perf_graph(&self) {
        self.paintable.toggle_perf_graph();
    }

    /// Handle to the canvas - used by the layers panel to drive
    /// add/reorder/visibility mutations through the same path the
    /// brush takes, keeping the renderer's per-layer images in sync.
    pub(crate) fn canvas(&self) -> Rc<RefCell<Canvas>> {
        Rc::clone(&self.canvas)
    }

    /// Cloneable handle to the "rebuild the displayed texture and
    /// queue a redraw" callable. Wire-time empty; `wire()` populates
    /// it once the GTK widget exists. The layers panel calls
    /// `request()` after every mutation so the viewport actually
    /// reflects the change.
    pub(crate) fn redraw_handle(&self) -> RedrawHandle {
        self.redraw.clone()
    }

    /// Live handle to the canvas-size cell. Returned as a clone so callers
    /// see future cropping/resizing without re-fetching.
    pub(crate) fn canvas_size_handle(&self) -> Rc<Cell<Size>> {
        Rc::clone(&self.canvas_size)
    }

    /// Live handle to the zoom factor (used for screen-space hit tolerances).
    pub(crate) fn zoom_handle(&self) -> Rc<Cell<f32>> {
        Rc::clone(&self.zoom)
    }

    /// Current canvas rotation in radians (persisted per document).
    pub(crate) fn rotation(&self) -> f32 {
        self.rotation.get()
    }

    /// Register the per-canvas info bar's refresh callback. Fired on every
    /// view change with the current canvas size + rotation.
    pub(crate) fn set_info_observer(&self, cb: Box<dyn Fn(Size, f32)>) {
        // Push the current state immediately so the bar starts correct.
        cb(self.canvas_size.get(), self.rotation.get());
        *self.info_observer.borrow_mut() = Some(cb);
    }

    fn view_sync(&self) -> ViewSync {
        ViewSync {
            pan: Rc::clone(&self.pan),
            zoom: Rc::clone(&self.zoom),
            rotation: Rc::clone(&self.rotation),
            canvas_size: Rc::clone(&self.canvas_size),
            paintable: self.paintable.clone(),
            info_observer: Rc::clone(&self.info_observer),
        }
    }

    fn rotation_animator(&self) -> RotationAnimator {
        RotationAnimator {
            sync: self.view_sync(),
            picture: Rc::clone(&self.picture),
            target: Rc::clone(&self.snap_target),
            installed: Rc::clone(&self.snap_tick_installed),
            last_time: Rc::clone(&self.snap_last_time),
        }
    }

    /// Rotate the view to an absolute angle (radians), pivoting around the
    /// viewport centre so the canvas point under the centre stays fixed.
    pub(crate) fn rotate_to(&self, new_theta: f32) {
        rotate_about_center(&self.pan, &self.zoom, &self.rotation, &self.picture, new_theta);
        self.view_sync().commit();
    }

    /// Set the rotation directly (used when loading a document). Pairs with a
    /// subsequent `zoom_fit`, which re-centres pan for the loaded angle.
    pub(crate) fn set_rotation_raw(&self, theta: f32) {
        self.rotation.set(theta);
        self.view_sync().commit();
    }

    pub(crate) fn zoom_in(&self) {
        let old = self.zoom.get();
        #[allow(clippy::cast_possible_truncation)]
        let new =
            (f64::from(old) * ZOOM_STEP).clamp(f64::from(MIN_ZOOM), f64::from(MAX_ZOOM)) as f32;
        self.zoom_toward(new);
    }

    pub(crate) fn zoom_out(&self) {
        let old = self.zoom.get();
        #[allow(clippy::cast_possible_truncation)]
        let new =
            (f64::from(old) / ZOOM_STEP).clamp(f64::from(MIN_ZOOM), f64::from(MAX_ZOOM)) as f32;
        self.zoom_toward(new);
    }

    pub(crate) fn zoom_fit(&self) {
        let (w, h) = self
            .picture
            .borrow()
            .as_ref()
            .map_or((0, 0), |p| (p.width(), p.height()));
        if w > 0 && h > 0 {
            fit_and_center(self, self.canvas_size.get(), w, h);
        }
    }

    /// Re-read the canvas size from the canvas and propagate it to the size
    /// cell + paintable, then refit zoom. Used after undo/redo of a crop,
    /// which mutates the canvas dimensions outside the normal crop path.
    pub(crate) fn resync_canvas_size(&self) {
        let new_size = self.canvas.borrow().size();
        if self.canvas_size.get() == new_size {
            return;
        }
        self.canvas_size.set(new_size);
        self.paintable
            .set_canvas_size(new_size.width, new_size.height);
        self.zoom_fit();
    }

    /// Apply a crop rectangle, resize the canvas, and update the paintable.
    /// Returns the new canvas size on success.
    pub(crate) fn apply_crop(&self, rect: CropRect) -> Option<Size> {
        let new_size = {
            let mut canvas = self.canvas.borrow_mut();
            match canvas.apply_crop(rect) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "apply_crop failed");
                    return None;
                }
            }
        };
        self.canvas_size.set(new_size);
        self.paintable
            .set_canvas_size(new_size.width, new_size.height);
        // Refresh the info bar's size readout (crop doesn't refit the zoom).
        self.view_sync().commit();
        Some(new_size)
    }

    /// Swap the canvas to a different size + layer set (component edit mode).
    /// Recreates the renderer, loads `layers`, updates the size cell + paintable,
    /// and refits the zoom. Returns false on renderer failure.
    pub(crate) fn load_layers_resized(
        &self,
        size: Size,
        layers: &[(String, String, bool, oxiedraw_core::document::BlendMode, f32, Vec<u8>)],
        active: Option<usize>,
    ) -> bool {
        {
            let mut canvas = self.canvas.borrow_mut();
            if let Err(e) = canvas.resize_and_replace_layers(size, layers, active) {
                tracing::error!(error = %e, "load_layers_resized failed");
                return false;
            }
        }
        self.canvas_size.set(size);
        self.paintable.set_canvas_size(size.width, size.height);
        self.zoom_fit();
        true
    }

    /// Map a widget-space point (picture coordinates) to canvas-pixel space
    /// using the current pan/zoom. Used by the component drag-drop target.
    pub(crate) fn widget_to_canvas_point(&self, x: f64, y: f64) -> Point {
        widget_to_canvas(x, y, &self.pan, &self.zoom, &self.rotation)
    }

    /// The canvas `Picture` widget, once wired. Used to read theme colours.
    pub(crate) fn picture_widget(&self) -> Option<gtk::Picture> {
        self.picture.borrow().clone()
    }

    fn zoom_toward(&self, new_zoom: f32) {
        let old_zoom = self.zoom.get();
        #[allow(clippy::cast_precision_loss)]
        let (cx, cy) = self
            .picture
            .borrow()
            .as_ref()
            .map_or((0.0, 0.0), |p| (p.width() as f32 / 2.0, p.height() as f32 / 2.0));
        // The pivot recompute is rotation-invariant: R*zoom*canvas == A - pan,
        // so the same ratio formula keeps the point under the centre fixed.
        let p = self.pan.get();
        let ratio = new_zoom / old_zoom;
        let new_pan = Point::new(
            (cx - p.x).mul_add(-ratio, cx),
            (cy - p.y).mul_add(-ratio, cy),
        );
        self.pan.set(new_pan);
        self.zoom.set(new_zoom);
        self.view_sync().commit();
    }
}

pub(crate) fn wire(
    picture: &gtk::Picture,
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
    history: &Rc<RefCell<oxiedraw_core::history::HistoryStack>>,
    toaster: &crate::toaster::Toaster,
    text_edit: &crate::text_edit::TextEdit,
    cursor_activates_transform: std::rc::Rc<dyn Fn()>,
) {
    *viewport.picture.borrow_mut() = Some(picture.clone());
    picture.set_paintable(Some(viewport.paintable()));

    // Populate the redraw handle now that we have a real widget. Any
    // code that mutates the canvas (brush handlers below, the layers
    // panel) can call `viewport.redraw_handle().request()` to drive a
    // re-present + texture rebuild + widget redraw via one path.
    {
        let canvas = Rc::clone(&viewport.canvas);
        let paintable = viewport.paintable.clone();
        let area = picture.clone();
        let cb: RedrawFn = Rc::new(move || {
            let mut canvas_ref = canvas.borrow_mut();
            present_into_paintable(&mut canvas_ref, &paintable, &area);
        });
        viewport.redraw.set(cb);
    }

    install_motion(
        picture, viewport, brush_engine, colors, tools, crop, transform, gradient, text_edit,
    );
    install_pan(picture, viewport);
    install_scroll(picture, viewport);
    install_pinch_zoom(picture, viewport);
    primary_drag::install_primary_drag(
        picture,
        viewport,
        brush_engine,
        colors,
        tools,
        crop,
        transform,
        selection,
        fill,
        shape,
        gradient,
        guide,
        history,
        toaster,
        text_edit,
        cursor_activates_transform,
    );
    install_centering_and_present(picture, viewport);
}

fn install_motion(
    area: &gtk::Picture,
    viewport: &Viewport,
    brush_engine: &BrushEngine,
    colors: &ColorState,
    tools: &ToolState,
    crop: &CropState,
    transform: &TransformState,
    gradient: &GradientState,
    text_edit: &crate::text_edit::TextEdit,
) {
    let motion = gtk::EventControllerMotion::new();
    let cursor_pos = Rc::clone(&viewport.cursor);
    let nav = Rc::clone(&viewport.nav);
    let pan = Rc::clone(&viewport.pan);
    let zoom = Rc::clone(&viewport.zoom);
    let rotation = Rc::clone(&viewport.rotation);
    let canvas = Rc::clone(&viewport.canvas);
    let paintable = viewport.paintable.clone();
    let tools_c = tools.clone();
    let crop = crop.clone();
    let transform = transform.clone();
    let gradient = gradient.clone();
    let brush_engine = brush_engine.clone();
    let colors = colors.clone();
    let area_c = area.clone();
    let text_edit = text_edit.clone();

    // Per-pointer history used to feed dynamics for the cursor preview:
    // - position + timestamp drive `speed`
    // - position delta drives `direction` (and `FakePenRotation`)
    // `last_direction` is sticky so the outline holds its rotation when
    // the pointer pauses instead of snapping back to 0.
    let last_motion = Rc::new(Cell::new(Option::<(Point, u64)>::None));
    let last_direction = Rc::new(Cell::new(0.0_f32));
    let last_motion_c = Rc::clone(&last_motion);
    let last_direction_c = Rc::clone(&last_direction);

    motion.connect_motion(move |ctrl, x, y| {
        #[allow(clippy::cast_possible_truncation)]
        cursor_pos.set(Point::new(x as f32, y as f32));

        // While a middle-mouse pan / zoom drag is active, leave the grab or
        // magnifier cursor in place and don't draw a brush outline.
        if nav.get() != NavDrag::None {
            paintable.set_brush_cursor(None, Point::ZERO);
            paintable.set_color_picker(None);
            paintable.set_gradient_cursor(None);
            return;
        }

        // Only the Gradient tool draws the ramp cursor; clear it otherwise.
        if !matches!(tools_c.active.get(), Tool::Fill(FillTool::Gradient)) {
            paintable.set_gradient_cursor(None);
        }

        match tools_c.active.get() {
            Tool::Crop => {
                let p = pan.get();
                let z = zoom.get();
                let rect_widget = crop.rect.get().map(|r| {
                    let n = r.normalized();
                    (
                        p.x + n.x * z,
                        p.y + n.y * z,
                        p.x + n.right() * z,
                        p.y + n.bottom() * z,
                    )
                });
                #[allow(clippy::cast_possible_truncation)]
                let handle = crop_geom::hit_test_widget(rect_widget, x as f32, y as f32);
                area_c.set_cursor_from_name(Some(crop_geom::cursor_name(handle)));
                paintable.set_brush_cursor(None, Point::ZERO);
                paintable.set_color_picker(None);
            }
            Tool::Transform => {
                if let Some(rect) = transform.rect.get() {
                    #[allow(clippy::cast_possible_truncation)]
                    let handle =
                        transform_geometry::hit_test(rect, x as f32, y as f32, &pan, &zoom, &rotation);
                    area_c.set_cursor_from_name(Some(transform_geometry::cursor_name(handle)));
                } else {
                    area_c.set_cursor_from_name(None);
                }
                paintable.set_brush_cursor(None, Point::ZERO);
                paintable.set_color_picker(None);
            }
            Tool::ColorPicker => {
                // Hide the OS pointer; the drawn eyedropper is the cursor.
                area_c.set_cursor_from_name(Some("none"));
                paintable.set_brush_cursor(None, Point::ZERO);
                let canvas_pos = widget_to_canvas(x, y, &pan, &zoom, &rotation);
                let color = sample_canvas_color(&canvas, canvas_pos);
                #[allow(clippy::cast_possible_truncation)]
                paintable.set_color_picker(Some(ColorPickerOverlay {
                    cursor: Point::new(x as f32, y as f32),
                    color,
                }));
            }
            Tool::Brush => {
                // Hide the OS pointer over the canvas - the brush
                // outline *is* the cursor. The hint covers the picture
                // area only; switching tools restores normal cursors.
                area_c.set_cursor_from_name(Some("none"));
                paintable.set_color_picker(None);
                let canvas_pos = widget_to_canvas(x, y, &pan, &zoom, &rotation);
                let time_ms = u64::from(ctrl.current_event_time());
                let (speed_px_ms, direction_rad) =
                    motion_kinematics(&last_motion_c, &last_direction_c, canvas_pos, time_ms);
                let (pressure, tilt_x, tilt_y, pen_rotation_rad) = device_axes(ctrl);
                let preset = brush_engine.active_brush();
                let ctx = StrokeContext {
                    preset: preset.id,
                    color: colors.current(),
                    size: brush_engine.size.get(),
                    opacity: brush_engine.opacity.get(),
                };
                let input = make_spawn_input(
                    pressure,
                    speed_px_ms,
                    direction_rad,
                    /* cumulative distance */ 0.0,
                    ctx.size,
                    stable_random_for(canvas_pos),
                    pen_rotation_rad,
                    tilt_x,
                    tilt_y,
                );
                let cursor = compute_brush_cursor(&preset, ctx, input, ctx.size);
                paintable.set_brush_cursor(Some(cursor), canvas_pos);
            }
            Tool::Text => {
                // Resize cursor over a handle of the box being edited;
                // otherwise the text/I-beam cursor.
                let canvas_pos = widget_to_canvas(x, y, &pan, &zoom, &rotation);
                let name = text_edit.cursor_for(canvas_pos).unwrap_or("text");
                area_c.set_cursor_from_name(Some(name));
                paintable.set_brush_cursor(None, Point::ZERO);
                paintable.set_color_picker(None);
            }
            Tool::Fill(FillTool::Gradient) => {
                // Hide the OS pointer; the drawn crosshair + ramp swatch is
                // the cursor (mirrors the color-picker eyedropper).
                area_c.set_cursor_from_name(Some("none"));
                paintable.set_brush_cursor(None, Point::ZERO);
                paintable.set_color_picker(None);
                #[allow(clippy::cast_possible_truncation)]
                paintable.set_gradient_cursor(Some(GradientCursorOverlay {
                    cursor: Point::new(x as f32, y as f32),
                    settings: gradient.resolve(&colors),
                }));
            }
            _ => {
                area_c.set_cursor_from_name(None);
                paintable.set_brush_cursor(None, Point::ZERO);
                paintable.set_color_picker(None);
            }
        }
    });

    // Clear the outline when the pointer leaves the picture so it
    // doesn't linger frozen at the last hover position.
    {
        let paintable = viewport.paintable.clone();
        motion.connect_leave(move |_| {
            paintable.set_brush_cursor(None, Point::ZERO);
            paintable.set_color_picker(None);
            paintable.set_gradient_cursor(None);
        });
    }

    area.add_controller(motion);
}

/// Update motion history and return `(speed_px_per_ms, direction_rad)`
/// derived from the delta since the previous sample. `last_direction`
/// is sticky: when the pointer pauses (or moves by less than a pixel)
/// the previously-observed direction is returned so the cursor outline
/// doesn't snap its rotation to zero on every idle frame.
fn motion_kinematics(
    last: &Rc<Cell<Option<(Point, u64)>>>,
    last_direction: &Rc<Cell<f32>>,
    pos: Point,
    time_ms: u64,
) -> (f32, f32) {
    let prev = last.replace(Some((pos, time_ms)));
    let Some((prev_pos, prev_time)) = prev else {
        return (0.0, last_direction.get());
    };
    let dt = time_ms.saturating_sub(prev_time);
    let dx = pos.x - prev_pos.x;
    let dy = pos.y - prev_pos.y;
    let dist = dx.hypot(dy);
    let speed = if dt == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        let dt_f = dt as f32;
        dist / dt_f
    };
    // Don't update direction on sub-pixel jitter - leaves the cursor
    // pointing the way it was last meaningfully moved.
    let direction = if dist > 0.5 {
        let d = dy.atan2(dx);
        last_direction.set(d);
        d
    } else {
        last_direction.get()
    };
    (speed, direction)
}

/// Read tablet axes (pressure / tilt / barrel rotation) from the
/// motion controller's current event. Falls back to `pressure = 1.0`
/// and zero for tilt / rotation when the device doesn't expose them
/// (typical mouse), matching what `sample_from` does on drag events.
fn device_axes(ctrl: &gtk::EventControllerMotion) -> (f32, f32, f32, f32) {
    let event = ctrl.current_event();
    let read = |axis: gdk::AxisUse, default: f32| -> f32 {
        #[allow(clippy::cast_possible_truncation)]
        event
            .as_ref()
            .and_then(|e| e.axis(axis))
            .map_or(default, |v| v as f32)
    };
    let pressure = read(gdk::AxisUse::Pressure, 1.0);
    let tilt_x = read(gdk::AxisUse::Xtilt, 0.0);
    let tilt_y = read(gdk::AxisUse::Ytilt, 0.0);
    // GDK reports rotation as a fraction of one revolution; the brush
    // engine wants radians.
    let rotation_rad = read(gdk::AxisUse::Rotation, 0.0) * std::f32::consts::TAU;
    (pressure, tilt_x, tilt_y, rotation_rad)
}

/// Deterministic-but-position-dependent unit value used for scatter /
/// random dynamics in the cursor preview. Picking it from the cursor
/// position means the preview jitters as the user moves, matching what
/// random scatter looks like during a real stroke.
fn stable_random_for(pos: Point) -> f32 {
    let bits = pos.x.to_bits() ^ pos.y.to_bits().rotate_left(13);
    let hashed = bits.wrapping_mul(0x9e37_79b9);
    #[allow(clippy::cast_precision_loss)]
    let v = (hashed >> 8) as f32;
    v / ((1u32 << 24) as f32)
}

/// Resolve a modifier-only keybind (e.g. "rotate-modifier") to its GDK mask, or
/// `None` if the user has unbound it. Loaded from settings at drag-begin so
/// changes in Preferences > Keybinds take effect on the next drag.
fn drag_modifier_mask(id: &str) -> Option<gdk::ModifierType> {
    let settings = crate::settings::AppSettings::load();
    let parts = crate::settings::keybinds::accel_parts_for(id, &settings)?;
    parts.iter().find_map(|p| match p.as_str() {
        "Shift" => Some(gdk::ModifierType::SHIFT_MASK),
        "Ctrl" => Some(gdk::ModifierType::CONTROL_MASK),
        "Alt" => Some(gdk::ModifierType::ALT_MASK),
        _ => None,
    })
}

/// Middle-mouse (or stylus pan) drag. Plain drag pans the canvas; holding Ctrl
/// turns it into a Krita-style continuous zoom centred on the drag origin;
/// holding the configured rotate modifier rotates the canvas about the viewport
/// centre (add the snap modifier for 45 deg steps).
fn install_pan(area: &gtk::Picture, viewport: &Viewport) {
    let drag = gtk::GestureDrag::new();
    drag.set_button(BUTTON_MIDDLE);

    {
        let last = Rc::clone(&viewport.pan_last_offset);
        let nav = Rc::clone(&viewport.nav);
        let nav_zoom_start = Rc::clone(&viewport.nav_zoom_start);
        let nav_anchor = Rc::clone(&viewport.nav_anchor);
        let nav_rotate_target = Rc::clone(&viewport.nav_rotate_target);
        let nav_rotate_last_angle = Rc::clone(&viewport.nav_rotate_last_angle);
        let nav_rotate_snap_mask = Rc::clone(&viewport.nav_rotate_snap_mask);
        let nav_rotate_snap_step = Rc::clone(&viewport.nav_rotate_snap_step);
        let zoom = Rc::clone(&viewport.zoom);
        let rotation = Rc::clone(&viewport.rotation);
        let animator = viewport.rotation_animator();
        let pump = viewport.render_pump.clone();
        let area_c = area.clone();
        drag.connect_drag_begin(move |gesture, start_x, start_y| {
            pump.arm();
            last.set(Point::ZERO);
            // Any in-flight snap ease from a previous drag ends here.
            animator.cancel();
            let state = gesture.current_event_state();
            let rotate_mask = drag_modifier_mask("rotate-modifier");
            let ctrl_held = state.contains(gdk::ModifierType::CONTROL_MASK);
            let rotate_held = rotate_mask.is_some_and(|m| state.contains(m));
            // Rotate takes precedence over the Ctrl-zoom when its modifier is
            // held (unless that modifier *is* Ctrl, in which case zoom wins).
            if rotate_held && rotate_mask != Some(gdk::ModifierType::CONTROL_MASK) {
                nav.set(NavDrag::Rotate);
                nav_rotate_target.set(rotation.get());
                // Resolve the snap modifier + step once here, off the per-event
                // path, so drag-update never re-reads settings from disk.
                nav_rotate_snap_mask.set(drag_modifier_mask("rotate-snap-modifier"));
                nav_rotate_snap_step.set(rotation_snap_rad());
                // Seed the angular tracker with the pointer's angle about the
                // viewport centre - rotation then follows the pointer like a
                // handle (Krita-style), not left/right travel.
                #[allow(clippy::cast_possible_truncation)]
                let (sx, sy) = (start_x as f32, start_y as f32);
                #[allow(clippy::cast_possible_truncation)]
                let (cx, cy) = (area_c.width() as f32 / 2.0, area_c.height() as f32 / 2.0);
                nav_anchor.set(Point::new(sx, sy));
                nav_rotate_last_angle.set((sy - cy).atan2(sx - cx));
                area_c.set_cursor_from_name(Some("crosshair"));
            } else if ctrl_held {
                nav.set(NavDrag::Zoom);
                nav_zoom_start.set(zoom.get());
                #[allow(clippy::cast_possible_truncation)]
                nav_anchor.set(Point::new(start_x as f32, start_y as f32));
                area_c.set_cursor_from_name(Some("zoom-in"));
            } else {
                nav.set(NavDrag::Pan);
                area_c.set_cursor_from_name(Some("grabbing"));
            }
        });
    }
    {
        let pan = Rc::clone(&viewport.pan);
        let last = Rc::clone(&viewport.pan_last_offset);
        let zoom = Rc::clone(&viewport.zoom);
        let rotation = Rc::clone(&viewport.rotation);
        let nav = Rc::clone(&viewport.nav);
        let nav_zoom_start = Rc::clone(&viewport.nav_zoom_start);
        let nav_anchor = Rc::clone(&viewport.nav_anchor);
        let nav_rotate_target = Rc::clone(&viewport.nav_rotate_target);
        let nav_rotate_last_angle = Rc::clone(&viewport.nav_rotate_last_angle);
        let nav_rotate_snap_mask = Rc::clone(&viewport.nav_rotate_snap_mask);
        let nav_rotate_snap_step = Rc::clone(&viewport.nav_rotate_snap_step);
        let sync = viewport.view_sync();
        let animator = viewport.rotation_animator();
        let picture = Rc::clone(&viewport.picture);
        let area_c = area.clone();
        drag.connect_drag_update(move |gesture, dx, dy| {
            #[allow(clippy::cast_possible_truncation)]
            let offset = Point::new(dx as f32, dy as f32);
            if nav.get() == NavDrag::Rotate {
                // Angular travel of the pointer around the viewport centre since
                // the last update, accumulated into the free target.
                let sp = nav_anchor.get();
                #[allow(clippy::cast_possible_truncation)]
                let (cx, cy) = (area_c.width() as f32 / 2.0, area_c.height() as f32 / 2.0);
                #[allow(clippy::cast_possible_truncation)]
                let cur_angle = (sp.y + dy as f32 - cy).atan2(sp.x + dx as f32 - cx);
                let d = oxiedraw_utils::math::wrap_pi(cur_angle - nav_rotate_last_angle.get());
                nav_rotate_last_angle.set(cur_angle);
                let target = nav_rotate_target.get() + d;
                nav_rotate_target.set(target);

                let snap_mask = nav_rotate_snap_mask.get();
                if snap_mask.is_some_and(|m| gesture.current_event_state().contains(m)) {
                    // Ease toward the snapped stop instead of jumping to it.
                    let step = nav_rotate_snap_step.get();
                    let snapped = (target / step).round() * step;
                    animator.animate_to(snapped);
                } else {
                    // Free rotation: cancel any in-flight ease and track directly.
                    animator.cancel();
                    rotate_about_center(&pan, &zoom, &rotation, &picture, target);
                    sync.commit();
                }
                return;
            }
            if nav.get() == NavDrag::Zoom {
                let old_zoom = zoom.get();
                let new_zoom = (f64::from(nav_zoom_start.get())
                    * 2f64.powf(-dy / ZOOM_DRAG_OCTAVE_PX))
                .clamp(f64::from(MIN_ZOOM), f64::from(MAX_ZOOM))
                    as f32;
                area_c.set_cursor_from_name(Some(if new_zoom >= old_zoom {
                    "zoom-in"
                } else {
                    "zoom-out"
                }));
                if (new_zoom - old_zoom).abs() < f32::EPSILON {
                    return;
                }
                let a = nav_anchor.get();
                let p = pan.get();
                let ratio = new_zoom / old_zoom;
                let new_pan = Point::new(
                    (a.x - p.x).mul_add(-ratio, a.x),
                    (a.y - p.y).mul_add(-ratio, a.y),
                );
                pan.set(new_pan);
                zoom.set(new_zoom);
            } else {
                let new_pan = pan_increment(pan.get(), last.get(), offset);
                last.set(offset);
                pan.set(new_pan);
            }
            sync.commit();
            // The render pump (armed at drag-begin) re-presents every frame, so
            // just updating the transform here is enough - no per-event present.
        });
    }
    {
        let nav = Rc::clone(&viewport.nav);
        let pump = viewport.render_pump.clone();
        let area_c = area.clone();
        drag.connect_drag_end(move |_, _, _| {
            nav.set(NavDrag::None);
            pump.disarm();
            // Drop the grab / zoom cursor; the next motion event restores
            // the tool's normal cursor.
            area_c.set_cursor_from_name(None);
        });
    }

    area.add_controller(drag);
}

/// Compute the new pan position from the current pan and a fresh cumulative
/// drag offset reported by `GtkGestureDrag::drag-update`.
///
/// The signal hands us the *cumulative* offset from the drag's start point,
/// not an incremental delta. Naively doing `new_pan = origin + offset` (with
/// `origin` captured at `drag-begin`) means any concurrent pan modification
/// - most notably the scroll-wheel zoom handler, which can fire from
/// touchpad kinetic scrolling well after the user has moved on to a
/// middle-mouse pan - gets overwritten on the next update and the canvas
/// "jumps" back to `origin + offset`. Applying only the increment since the
/// previous update keeps both effects composing additively.
fn pan_increment(current_pan: Point, last_offset: Point, offset: Point) -> Point {
    Point::new(
        current_pan.x + (offset.x - last_offset.x),
        current_pan.y + (offset.y - last_offset.y),
    )
}

/// Set the rotation to `theta` while keeping the canvas point under the
/// viewport centre fixed (recomputes `pan`). Cell-based twin of
/// [`Viewport::rotate_to`] for use inside gesture closures.
fn rotate_about_center(
    pan: &Rc<Cell<Point>>,
    zoom: &Rc<Cell<f32>>,
    rotation: &Rc<Cell<f32>>,
    picture: &Rc<RefCell<Option<gtk::Picture>>>,
    theta: f32,
) {
    #[allow(clippy::cast_precision_loss)]
    let (cx, cy) = picture
        .borrow()
        .as_ref()
        .map_or((0.0, 0.0), |p| (p.width() as f32 / 2.0, p.height() as f32 / 2.0));
    let pivot = widget_to_canvas_xf(cx, cy, pan.get(), zoom.get(), rotation.get());
    rotation.set(theta);
    let z = zoom.get();
    let (s, co) = theta.sin_cos();
    let rx = co.mul_add(pivot.x, -s * pivot.y);
    let ry = s.mul_add(pivot.x, co * pivot.y);
    pan.set(Point::new(cx - z * rx, cy - z * ry));
}

/// Scroll-wheel / touchpad two-finger scroll handling.
///
/// A mouse wheel (`ScrollUnit::Wheel`) zooms toward the cursor, as before.
/// A touchpad (`ScrollUnit::Surface`) instead pans the canvas: horizontal
/// scroll pans horizontally, vertical scroll pans vertically - matching the
/// native two-finger scroll feel. Touchpad pinch-to-zoom is handled
/// separately by [`install_pinch_zoom`].
fn install_scroll(area: &gtk::Picture, viewport: &Viewport) {
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let pan = Rc::clone(&viewport.pan);
    let zoom = Rc::clone(&viewport.zoom);
    let cursor = Rc::clone(&viewport.cursor);
    let sync = viewport.view_sync();
    scroll.connect_scroll(move |controller, dx, dy| {
        if controller.unit() == gdk::ScrollUnit::Surface {
            // Touchpad: pan opposite the finger delta so content tracks
            // the fingers, exactly like scrolling a document.
            #[allow(clippy::cast_possible_truncation)]
            let delta = Point::new(dx as f32, dy as f32);
            if delta.x == 0.0 && delta.y == 0.0 {
                return glib::Propagation::Stop;
            }
            let p = pan.get();
            let new_pan = Point::new(
                delta.x.mul_add(-TOUCHPAD_PAN_SPEED, p.x),
                delta.y.mul_add(-TOUCHPAD_PAN_SPEED, p.y),
            );
            pan.set(new_pan);
            sync.commit();
            return glib::Propagation::Stop;
        }

        // Mouse wheel: zoom toward the cursor.
        let old_zoom = zoom.get();
        #[allow(clippy::cast_possible_truncation)]
        let new_zoom = (f64::from(old_zoom) * ZOOM_STEP.powf(-dy))
            .clamp(f64::from(MIN_ZOOM), f64::from(MAX_ZOOM)) as f32;
        if (new_zoom - old_zoom).abs() < f32::EPSILON {
            return glib::Propagation::Stop;
        }

        let c = cursor.get();
        let p = pan.get();
        let ratio = new_zoom / old_zoom;
        let new_pan = Point::new(
            (c.x - p.x).mul_add(-ratio, c.x),
            (c.y - p.y).mul_add(-ratio, c.y),
        );
        pan.set(new_pan);
        zoom.set(new_zoom);
        sync.commit();
        glib::Propagation::Stop
    });
    area.add_controller(scroll);
}

/// Touchpad pinch-to-zoom.
///
/// We read touchpad-pinch events through a `GtkEventControllerLegacy` rather
/// than a `GtkGestureZoom`. A gesture participates in GTK's gesture-recognition
/// arbitration and, sharing the widget with the drawing `GtkGestureDrag`, held
/// back stylus motion events long enough to make the pen visibly jittery. The
/// legacy controller just observes each event and lets everything it doesn't
/// consume flow through untouched, so the stylus path is unaffected.
///
/// `pinch_scale()` is cumulative relative to the gesture start (1.0 at begin),
/// mapped onto an absolute zoom pivoting around the last cursor position - the
/// same anchor the mouse-wheel zoom uses.
fn install_pinch_zoom(area: &gtk::Picture, viewport: &Viewport) {
    let legacy = gtk::EventControllerLegacy::new();
    let pan = Rc::clone(&viewport.pan);
    let zoom = Rc::clone(&viewport.zoom);
    let cursor = Rc::clone(&viewport.cursor);
    let pinch_start = Rc::clone(&viewport.pinch_zoom_start);
    let sync = viewport.view_sync();
    legacy.connect_event(move |_, event| {
        if event.event_type() != gdk::EventType::TouchpadPinch {
            return glib::Propagation::Proceed;
        }
        let Some(pinch) = event.downcast_ref::<gdk::TouchpadEvent>() else {
            return glib::Propagation::Proceed;
        };
        match pinch.gesture_phase() {
            gdk::TouchpadGesturePhase::Begin => pinch_start.set(zoom.get()),
            gdk::TouchpadGesturePhase::Update => {
                let old_zoom = zoom.get();
                #[allow(clippy::cast_possible_truncation)]
                let new_zoom = (f64::from(pinch_start.get()) * pinch.pinch_scale())
                    .clamp(f64::from(MIN_ZOOM), f64::from(MAX_ZOOM)) as f32;
                if (new_zoom - old_zoom).abs() >= f32::EPSILON {
                    let c = cursor.get();
                    let p = pan.get();
                    let ratio = new_zoom / old_zoom;
                    let new_pan = Point::new(
                        (c.x - p.x).mul_add(-ratio, c.x),
                        (c.y - p.y).mul_add(-ratio, c.y),
                    );
                    pan.set(new_pan);
                    zoom.set(new_zoom);
                    sync.commit();
                }
            }
            _ => {}
        }
        glib::Propagation::Stop
    });
    area.add_controller(legacy);
}

/// One-time hookup that sets the initial pan / zoom (centred fit) once
/// the widget has a non-zero allocation, then publishes the first
/// texture so the canvas is visible before the user touches anything.
fn install_centering_and_present(area: &gtk::Picture, viewport: &Viewport) {
    let viewport = viewport.clone();
    area.add_tick_callback(move |area, _clock| {
        let w = area.width();
        let h = area.height();
        if w <= 0 || h <= 0 {
            return glib::ControlFlow::Continue;
        }
        if !viewport.centered.get() {
            fit_and_center(&viewport, viewport.canvas_size.get(), w, h);
        }
        let mut canvas = viewport.canvas.borrow_mut();
        present_into_paintable(&mut canvas, &viewport.paintable, area);
        glib::ControlFlow::Break
    });
}

/// Push the latest canvas pixels to the paintable.
///
/// `gdk::DmabufTexture` snapshots the dmabuf contents at *build* time
/// (not live), so we have to rebuild a fresh texture every time the
/// canvas changes. We pass the previous texture to
/// `set_update_texture` so GTK knows this is a successor - the
/// renderer can then update its internal caches incrementally instead
/// of treating it as an unrelated import. After publishing, we
/// explicitly `queue_draw` the widget; relying on `invalidate_contents`
/// alone exhibited a "drawing not shown until first pan/zoom" bug on
/// AMD/RADV during bringup.
pub(super) fn present_into_paintable(
    canvas: &mut Canvas,
    paintable: &CanvasPaintable,
    area: &gtk::Picture,
) {
    let desc = match canvas.present() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "canvas.present failed");
            return;
        }
    };
    // Feed the perf overlay (only worth polling the GPU timestamps when shown).
    if paintable.perf_enabled() {
        paintable.record_gpu_timings(canvas.frame_timings());
    }
    apply_descriptor_to_paintable(&desc, canvas.last_present_region(), paintable, area);
}

/// Build a dmabuf texture from `desc` and hand it to the paintable. `damage`
/// is the region the present rewrote, in canvas pixels (`None` = all of it).
pub(super) fn apply_descriptor_to_paintable(
    desc: &DmabufDescriptor,
    damage: Option<(i32, i32, u32, u32)>,
    paintable: &CanvasPaintable,
    area: &gtk::Picture,
) {
    let previous = paintable.texture();
    match build_texture(area, desc, previous.as_ref(), damage) {
        Ok(texture) => paintable.set_texture(Some(texture)),
        Err(e) => tracing::error!(error = %e, "dmabuf texture build failed"),
    }
    area.queue_draw();
}

/// Build a `gdk::DmabufTexture` from a renderer descriptor. The fd is
/// `dup`ed so GTK owns its own copy; the `Arc` in the descriptor stays
/// with the renderer. On texture drop the release callback closes
/// GTK's fd.
///
/// `previous`, when `Some`, is passed via `set_update_texture` so GTK
/// recognises this build as a successor of that texture and can do
/// partial-update bookkeeping internally. `damage` pairs with it: without a
/// region GSK assumes the whole texture changed and re-renders the widget.
fn build_texture(
    area: &gtk::Picture,
    desc: &DmabufDescriptor,
    previous: Option<&gdk::Texture>,
    damage: Option<(i32, i32, u32, u32)>,
) -> Result<gdk::Texture, glib::Error> {
    let display = area.display();
    let dup =
        desc.fd.as_ref().try_clone().map_err(|e| {
            glib::Error::new(glib::FileError::Failed, &format!("dup dmabuf fd: {e}"))
        })?;
    let raw = dup.as_raw_fd();
    let mut builder = gdk::DmabufTextureBuilder::new()
        .set_display(&display)
        .set_width(desc.width)
        .set_height(desc.height)
        .set_fourcc(desc.fourcc)
        .set_modifier(desc.modifier)
        .set_premultiplied(true)
        .set_n_planes(1);
    // Only chain to the previous texture when it has the same dimensions.
    // `set_update_texture` is a "successor of that memory" hint; after a canvas
    // resize the previous texture is a different size, and feeding GTK a
    // mismatched successor wedges its dmabuf-import reuse into a degraded
    // per-frame path that paces at every-3rd-vsync (the post-resize 24ms cap).
    if let Some(prev) = previous
        && prev.width() == i32::try_from(desc.width).unwrap_or(-1)
        && prev.height() == i32::try_from(desc.height).unwrap_or(-1)
    {
        builder = builder.set_update_texture(Some(prev));
        // Only meaningful alongside a chained predecessor: it names the pixels
        // that differ from it.
        if let Some((x, y, w, h)) = damage
            && let (Ok(w), Ok(h)) = (i32::try_from(w), i32::try_from(h))
        {
            let rect = gtk::cairo::RectangleInt::new(x, y, w, h);
            builder = builder.set_update_region(Some(&gtk::cairo::Region::create_rectangle(&rect)));
        }
    }
    // SAFETY: `dup` (and thus `raw`) is kept alive by the release
    // closure below until GTK drops the texture.
    let builder = unsafe { builder.set_fd(0, raw) };
    let builder = builder
        .set_offset(0, desc.offset)
        .set_stride(0, desc.stride);
    // SAFETY: `dup` outlives the texture via the move into the closure.
    let texture = unsafe {
        builder.build_with_release_func(move || {
            drop(dup);
        })?
    };
    Ok(texture)
}

pub(super) fn sample_from(gesture: &gtk::GestureDrag, canvas_pos: Point) -> InputSample {
    InputSample {
        position: canvas_pos,
        pressure: pressure_from(gesture),
        tilt_x: axis_from(gesture, gdk::AxisUse::Xtilt, 0.0),
        tilt_y: axis_from(gesture, gdk::AxisUse::Ytilt, 0.0),
        // GDK reports the rotation axis as a fraction of a full
        // revolution (`0.0..=1.0`); the brush engine wants radians.
        rotation: axis_from(gesture, gdk::AxisUse::Rotation, 0.0)
            * std::f32::consts::TAU,
        time_ms: u64::from(gesture.current_event_time()),
    }
}

/// Sample the visible composited color at a canvas-space position. Floors
/// to the pixel under the cursor and reads it back from the GPU. Returns
/// `None` when the position is off-canvas.
pub(super) fn sample_canvas_color(canvas: &Rc<RefCell<Canvas>>, canvas_pos: Point) -> Option<Color> {
    if canvas_pos.x < 0.0 || canvas_pos.y < 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let px = canvas_pos.x.floor() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let py = canvas_pos.y.floor() as u32;
    canvas.borrow_mut().pick_color(px, py)
}

pub(super) fn widget_to_canvas(
    x: f64,
    y: f64,
    pan: &Rc<Cell<Point>>,
    zoom: &Rc<Cell<f32>>,
    rotation: &Rc<Cell<f32>>,
) -> Point {
    #[allow(clippy::cast_possible_truncation)]
    let (xf, yf) = (x as f32, y as f32);
    widget_to_canvas_xf(xf, yf, pan.get(), zoom.get(), rotation.get())
}

/// Invert `widget = pan + zoom * R(theta) * canvas` for a widget-space point:
/// undo the pan, rotate by `-theta`, and divide out the zoom.
pub(super) fn widget_to_canvas_xf(x: f32, y: f32, pan: Point, zoom: f32, rotation: f32) -> Point {
    let z = zoom.max(f32::EPSILON);
    let dx = x - pan.x;
    let dy = y - pan.y;
    let (s, c) = rotation.sin_cos();
    // R(-theta) * (dx, dy) / z
    Point::new(c.mul_add(dx, s * dy) / z, (-s).mul_add(dx, c * dy) / z)
}

fn pressure_from(gesture: &gtk::GestureDrag) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    gesture
        .current_event()
        .and_then(|e| e.axis(gdk::AxisUse::Pressure))
        .map_or(1.0, |p| p as f32)
}

/// Read an arbitrary GDK device axis from the current event, falling
/// back to `default` when the device doesn't expose that axis.
fn axis_from(gesture: &gtk::GestureDrag, axis: gdk::AxisUse, default: f32) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    gesture
        .current_event()
        .and_then(|e| e.axis(axis))
        .map_or(default, |v| v as f32)
}

fn fit_and_center(viewport: &Viewport, canvas_size: Size, w: i32, h: i32) {
    let area_w = f64::from(w);
    let area_h = f64::from(h);
    let cw = f64::from(canvas_size.width);
    let ch = f64::from(canvas_size.height);
    let fit = (area_w / cw).min(area_h / ch);
    #[allow(clippy::cast_possible_truncation)]
    let z = (fit * DEFAULT_FIT_RATIO) as f32;
    let z_clamped = z.clamp(MIN_ZOOM, MAX_ZOOM);
    viewport.zoom.set(z_clamped);
    // Map the canvas centre to the viewport centre for the current rotation:
    // pan = C_view - zoom * R(theta) * canvas_centre.
    #[allow(clippy::cast_possible_truncation)]
    let (view_cx, view_cy) = (area_w as f32 / 2.0, area_h as f32 / 2.0);
    #[allow(clippy::cast_possible_truncation)]
    let (ccx, ccy) = (cw as f32 / 2.0, ch as f32 / 2.0);
    let (s, co) = viewport.rotation.get().sin_cos();
    let rx = co.mul_add(ccx, -s * ccy);
    let ry = s.mul_add(ccx, co * ccy);
    let new_pan = Point::new(z_clamped.mul_add(-rx, view_cx), z_clamped.mul_add(-ry, view_cy));
    viewport.pan.set(new_pan);
    viewport.centered.set(true);
    viewport.view_sync().commit();
    tracing::debug!(
        zoom = z_clamped,
        pan_x = new_pan.x,
        pan_y = new_pan.y,
        "viewport centered"
    );
}

#[cfg(test)]
mod tests {
    use super::{Point, pan_increment};

    /// Simulate a typical pan drag: GTK sends cumulative offsets that grow
    /// monotonically. The pan should track the cumulative drag distance.
    #[test]
    fn pan_increment_tracks_cumulative_drag() {
        let start = Point::new(100.0, 50.0);
        let mut last = Point::ZERO;
        let mut pan = start;

        for &offset in &[(5.0, 3.0), (12.0, 8.0), (20.0, 15.0)] {
            let off = Point::new(offset.0, offset.1);
            pan = pan_increment(pan, last, off);
            last = off;
        }

        // Total offset applied = the last cumulative offset.
        assert!((pan.x - (start.x + 20.0)).abs() < 1e-4);
        assert!((pan.y - (start.y + 15.0)).abs() < 1e-4);
    }

    /// The bug: at high zoom a concurrent scroll-zoom (or touchpad kinetic
    /// scroll residue) shifts `pan` between drag updates. With the old
    /// cumulative-offset code the next update would snap pan back to
    /// `origin + offset`. With incremental deltas the zoom-induced shift
    /// must survive.
    #[test]
    fn pan_increment_preserves_external_pan_changes() {
        let mut pan = Point::new(0.0, 0.0);
        let mut last = Point::ZERO;

        // First drag update: cumulative offset (10, 5).
        let off = Point::new(10.0, 5.0);
        pan = pan_increment(pan, last, off);
        last = off;
        assert!((pan.x - 10.0).abs() < 1e-4);

        // Concurrent scroll-zoom shifts pan by a large amount (simulating the
        // cursor-anchored zoom recomputation at very high zoom).
        pan = Point::new(pan.x - 500.0, pan.y + 200.0);

        // Next drag update: cumulative offset (15, 8) - i.e. the user
        // continued moving by (5, 3) since the previous update.
        let off2 = Point::new(15.0, 8.0);
        pan = pan_increment(pan, last, off2);

        // The zoom shift survives, with only the incremental (5, 3) added.
        assert!((pan.x - (10.0 - 500.0 + 5.0)).abs() < 1e-4);
        assert!((pan.y - (5.0 + 200.0 + 3.0)).abs() < 1e-4);
    }

    /// A duplicate update with the same cumulative offset (GTK can emit
    /// these when nothing actually moved but the gesture refreshes) must be
    /// a no-op for the pan.
    #[test]
    fn pan_increment_idempotent_for_duplicate_offset() {
        let mut pan = Point::new(42.0, 17.0);
        let last = Point::new(10.0, 5.0);
        let off = Point::new(10.0, 5.0);
        pan = pan_increment(pan, last, off);
        assert!((pan.x - 42.0).abs() < 1e-4);
        assert!((pan.y - 17.0).abs() < 1e-4);
    }

    /// After drag-begin resets `last` to ZERO, a fresh drag continues from
    /// wherever pan happens to be at that moment - not from the value it had
    /// at the end of the previous drag.
    #[test]
    fn pan_increment_resumes_after_new_drag_begin() {
        // First drag ended at pan = (30, 30); something else then moves
        // pan in between drags (e.g. zoom_fit) to (200, 100).
        let mut pan = Point::new(200.0, 100.0);

        // New drag begin: last is reset.
        let mut last = Point::ZERO;
        // First update of the new drag.
        let off = Point::new(7.0, -4.0);
        pan = pan_increment(pan, last, off);
        last = off;

        assert!((pan.x - 207.0).abs() < 1e-4);
        assert!((pan.y - 96.0).abs() < 1e-4);
        assert_eq!(last, off);
    }
}
