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
use oxiedraw_core::tools::{
    CropRect, CropState, FillState, SelectionState, ShapeState, Tool, ToolState, TransformState,
};
use oxiedraw_utils::geometry::{Point, Size};

use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use oxiedraw_core::color::Color;

use crate::canvas_paintable::{CanvasPaintable, ColorPickerOverlay};

pub(super) const BUTTON_PRIMARY: u32 = 1;
const BUTTON_MIDDLE: u32 = 2;
const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 32.0;
const ZOOM_STEP: f64 = 1.1;
const DEFAULT_FIT_RATIO: f64 = 0.5;
/// Widget pixels of vertical drag that change the zoom by one octave
/// (factor of 2) during a Ctrl+middle-drag zoom, Krita-style.
const ZOOM_DRAG_OCTAVE_PX: f64 = 150.0;

/// Which navigation gesture (if any) the middle-mouse drag is currently
/// performing. Drives both the gesture handlers and the cursor that the
/// motion handler must leave alone while a nav drag is in progress.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NavDrag {
    None,
    Pan,
    Zoom,
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
    centered: Rc<Cell<bool>>,
    canvas: Rc<RefCell<Canvas>>,
    paintable: CanvasPaintable,
    redraw: RedrawHandle,
    canvas_size: Rc<Cell<Size>>,
    picture: Rc<RefCell<Option<gtk::Picture>>>,
}

impl std::fmt::Debug for Viewport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Viewport")
            .field("pan", &self.pan.get())
            .field("zoom", &self.zoom.get())
            .finish_non_exhaustive()
    }
}

impl Viewport {
    pub(crate) fn new(canvas_size: Size, layers: LayerState) -> Self {
        let canvas = Canvas::new(canvas_size, layers).expect("Vulkan canvas init");
        let paintable = CanvasPaintable::new(canvas_size.width, canvas_size.height);
        Self {
            pan: Rc::new(Cell::new(Point::ZERO)),
            pan_last_offset: Rc::new(Cell::new(Point::ZERO)),
            zoom: Rc::new(Cell::new(1.0)),
            cursor: Rc::new(Cell::new(Point::ZERO)),
            nav: Rc::new(Cell::new(NavDrag::None)),
            nav_zoom_start: Rc::new(Cell::new(1.0)),
            nav_anchor: Rc::new(Cell::new(Point::ZERO)),
            centered: Rc::new(Cell::new(false)),
            canvas: Rc::new(RefCell::new(canvas)),
            paintable,
            redraw: RedrawHandle::default(),
            canvas_size: Rc::new(Cell::new(canvas_size)),
            picture: Rc::new(RefCell::new(None)),
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
        Some(new_size)
    }

    /// Swap the canvas to a different size + layer set (component edit mode).
    /// Recreates the renderer, loads `layers`, updates the size cell + paintable,
    /// and refits the zoom. Returns false on renderer failure.
    pub(crate) fn load_layers_resized(
        &self,
        size: Size,
        layers: &[(String, String, bool, Vec<u8>)],
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
        widget_to_canvas(x, y, &self.pan, &self.zoom)
    }

    fn zoom_toward(&self, new_zoom: f32) {
        let old_zoom = self.zoom.get();
        #[allow(clippy::cast_precision_loss)]
        let (cx, cy) = self
            .picture
            .borrow()
            .as_ref()
            .map_or((0.0, 0.0), |p| (p.width() as f32 / 2.0, p.height() as f32 / 2.0));
        let p = self.pan.get();
        let ratio = new_zoom / old_zoom;
        let new_pan = Point::new(
            (cx - p.x).mul_add(-ratio, cx),
            (cy - p.y).mul_add(-ratio, cy),
        );
        self.pan.set(new_pan);
        self.zoom.set(new_zoom);
        self.paintable.set_transform(new_pan.x, new_pan.y, new_zoom);
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
        picture, viewport, brush_engine, colors, tools, crop, transform, text_edit,
    );
    install_pan(picture, viewport);
    install_zoom(picture, viewport);
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
    text_edit: &crate::text_edit::TextEdit,
) {
    let motion = gtk::EventControllerMotion::new();
    let cursor_pos = Rc::clone(&viewport.cursor);
    let nav = Rc::clone(&viewport.nav);
    let pan = Rc::clone(&viewport.pan);
    let zoom = Rc::clone(&viewport.zoom);
    let canvas = Rc::clone(&viewport.canvas);
    let paintable = viewport.paintable.clone();
    let tools_c = tools.clone();
    let crop = crop.clone();
    let transform = transform.clone();
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
            return;
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
                    let handle = transform_geometry::hit_test(rect, x as f32, y as f32, &pan, &zoom);
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
                let canvas_pos = widget_to_canvas(x, y, &pan, &zoom);
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
                let canvas_pos = widget_to_canvas(x, y, &pan, &zoom);
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
                let canvas_pos = widget_to_canvas(x, y, &pan, &zoom);
                let name = text_edit.cursor_for(canvas_pos).unwrap_or("text");
                area_c.set_cursor_from_name(Some(name));
                paintable.set_brush_cursor(None, Point::ZERO);
                paintable.set_color_picker(None);
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

/// Middle-mouse drag. Plain drag pans the canvas (grabbing-hand cursor);
/// holding Ctrl turns it into a Krita-style continuous zoom centred on the
/// drag origin (magnifier cursor), dragging up to zoom in, down to zoom out.
fn install_pan(area: &gtk::Picture, viewport: &Viewport) {
    let drag = gtk::GestureDrag::new();
    drag.set_button(BUTTON_MIDDLE);

    {
        let last = Rc::clone(&viewport.pan_last_offset);
        let nav = Rc::clone(&viewport.nav);
        let nav_zoom_start = Rc::clone(&viewport.nav_zoom_start);
        let nav_anchor = Rc::clone(&viewport.nav_anchor);
        let zoom = Rc::clone(&viewport.zoom);
        let area_c = area.clone();
        drag.connect_drag_begin(move |gesture, start_x, start_y| {
            last.set(Point::ZERO);
            let ctrl_held = gesture
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK);
            if ctrl_held {
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
        let paintable = viewport.paintable.clone();
        let zoom = Rc::clone(&viewport.zoom);
        let nav = Rc::clone(&viewport.nav);
        let nav_zoom_start = Rc::clone(&viewport.nav_zoom_start);
        let nav_anchor = Rc::clone(&viewport.nav_anchor);
        let area_c = area.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            #[allow(clippy::cast_possible_truncation)]
            let offset = Point::new(dx as f32, dy as f32);
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
                paintable.set_transform(new_pan.x, new_pan.y, new_zoom);
            } else {
                let new_pan = pan_increment(pan.get(), last.get(), offset);
                last.set(offset);
                pan.set(new_pan);
                paintable.set_transform(new_pan.x, new_pan.y, zoom.get());
            }
        });
    }
    {
        let nav = Rc::clone(&viewport.nav);
        let area_c = area.clone();
        drag.connect_drag_end(move |_, _, _| {
            nav.set(NavDrag::None);
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

fn install_zoom(area: &gtk::Picture, viewport: &Viewport) {
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    let pan = Rc::clone(&viewport.pan);
    let zoom = Rc::clone(&viewport.zoom);
    let cursor = Rc::clone(&viewport.cursor);
    let paintable = viewport.paintable.clone();
    scroll.connect_scroll(move |_, _dx, dy| {
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
        paintable.set_transform(new_pan.x, new_pan.y, new_zoom);
        glib::Propagation::Stop
    });
    area.add_controller(scroll);
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
    apply_descriptor_to_paintable(&desc, paintable, area);
}

/// Build a dmabuf texture from `desc` and hand it to the paintable. The
/// descriptor-application tail of [`present_into_paintable`], split out so
/// the combined stamp+present path can reuse it.
pub(super) fn apply_descriptor_to_paintable(
    desc: &DmabufDescriptor,
    paintable: &CanvasPaintable,
    area: &gtk::Picture,
) {
    let previous = paintable.texture();
    match build_texture(area, desc, previous.as_ref()) {
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
/// partial-update bookkeeping internally.
fn build_texture(
    area: &gtk::Picture,
    desc: &DmabufDescriptor,
    previous: Option<&gdk::Texture>,
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
    if let Some(prev) = previous {
        builder = builder.set_update_texture(Some(prev));
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
) -> Point {
    let p = pan.get();
    let z = zoom.get();
    #[allow(clippy::cast_possible_truncation)]
    let (xf, yf) = (x as f32, y as f32);
    Point::new((xf - p.x) / z, (yf - p.y) / z)
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
    let z64 = f64::from(z_clamped);
    let cx = cw.mul_add(-z64, area_w) / 2.0;
    let cy = ch.mul_add(-z64, area_h) / 2.0;
    viewport.zoom.set(z_clamped);
    #[allow(clippy::cast_possible_truncation)]
    let new_pan = Point::new(cx as f32, cy as f32);
    viewport.pan.set(new_pan);
    viewport.centered.set(true);
    viewport
        .paintable
        .set_transform(new_pan.x, new_pan.y, z_clamped);
    tracing::debug!(
        zoom = z_clamped,
        pan_x = cx,
        pan_y = cy,
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
