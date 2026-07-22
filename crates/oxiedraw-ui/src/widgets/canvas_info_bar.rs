//! Per-canvas bottom info strip: canvas size + a live rotation readout with a
//! draggable "rotator" dial (drag to rotate the view, double-click to reset).
//!
//! One instance lives under each document's canvas. The viewport pushes updates
//! through [`CanvasInfoBar::update`] on every pan/zoom/rotation change.
//!
//! The rotation needle *and* the numeric angle are drawn inside one
//! `DrawingArea`, not a `GtkLabel`. Updating a label's text mid-drag queues a
//! resize that GTK propagates up and re-allocates the canvas `Picture` under the
//! pen, cancelling the stylus grab (the same trap the crop tool documents). A
//! `DrawingArea` only ever `queue_draw`s, so the readout can track live without
//! disturbing an in-flight rotate drag.

use std::cell::Cell;
use std::f64::consts::FRAC_PI_2;
use std::rc::Rc;

use oxiedraw_utils::geometry::Size;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::prelude::*;

/// Square (px) reserved for the compass at the left of the rotator area.
const DIAL_BOX: i32 = 18;
/// Extra width (px) for the "X.XX deg" text.
const TEXT_W: i32 = 76;

#[derive(Clone)]
pub(crate) struct CanvasInfoBar {
    root: gtk::Box,
    size_label: gtk::Label,
    rotator: gtk::DrawingArea,
    /// Current rotation (radians) mirrored for the rotator's draw function.
    angle: Rc<Cell<f32>>,
}

impl CanvasInfoBar {
    /// Build the strip. `on_rotate(theta_radians)` is invoked while the user
    /// drags the dial and on a double-click reset (with `0.0`).
    pub(crate) fn new(on_rotate: Rc<dyn Fn(f32)>) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["sidebar"])
            .build();
        root.set_margin_start(8);
        root.set_margin_end(8);
        root.set_margin_top(1);
        root.set_margin_bottom(1);

        let size_label = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .build();

        let spacer = gtk::Box::builder().hexpand(true).build();

        let angle = Rc::new(Cell::new(0.0_f32));
        let rotator = gtk::DrawingArea::builder()
            .content_width(DIAL_BOX + TEXT_W)
            .content_height(DIAL_BOX)
            .tooltip_text("Drag to rotate the canvas; double-click to reset")
            .build();
        rotator.set_cursor_from_name(Some("grab"));
        {
            let angle = Rc::clone(&angle);
            rotator.set_draw_func(move |area, cr, w, h| draw_rotator(area, cr, w, h, angle.get()));
        }

        install_dial_gestures(&rotator, &on_rotate);

        root.append(&size_label);
        root.append(&spacer);
        root.append(&rotator);

        Self {
            root,
            size_label,
            rotator,
            angle,
        }
    }

    pub(crate) fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    /// Build the view-change observer for [`Viewport::set_info_observer`]. It
    /// captures only `WeakRef`s to the widgets (plus the cheap angle cell), never
    /// a strong handle to the bar. This matters: the observer lives inside the
    /// viewport, and the rotator dial's gesture already holds the viewport, so a
    /// strong capture here would form a cycle that leaks the whole document (its
    /// Vulkan canvas included) after the tab is closed.
    pub(crate) fn observer(&self) -> Box<dyn Fn(Size, f32)> {
        let size_label = self.size_label.downgrade();
        let rotator = self.rotator.downgrade();
        let angle = Rc::clone(&self.angle);
        Box::new(move |size, rotation| {
            angle.set(rotation);
            if let Some(rotator) = rotator.upgrade() {
                rotator.queue_draw();
            }
            // Only touch the label when the text actually changes: set_text
            // queues a resize, and during a rotate drag the size is constant, so
            // this stays a no-op and can't re-allocate the canvas Picture under
            // the pen.
            if let Some(label) = size_label.upgrade() {
                let text = format!("{} x {} px", size.width, size.height);
                if label.text().as_str() != text {
                    label.set_text(&text);
                }
            }
        })
    }
}

/// Drag maps the pointer's angle around the dial centre to an absolute canvas
/// rotation; a double-click resets to 0.
fn install_dial_gestures(rotator: &gtk::DrawingArea, on_rotate: &Rc<dyn Fn(f32)>) {
    let drag = gtk::GestureDrag::new();
    let start = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    {
        let start = Rc::clone(&start);
        drag.connect_drag_begin(move |_, x, y| start.set((x, y)));
    }
    {
        let start = Rc::clone(&start);
        let on_rotate = Rc::clone(on_rotate);
        drag.connect_drag_update(move |gesture, dx, dy| {
            // Dead zone: ignore sub-threshold travel so the tiny jitter during a
            // click (including the two presses of a double-click reset) doesn't
            // snap the rotation to the click position and clobber the reset.
            if dx.hypot(dy) < 4.0 {
                return;
            }
            let (sx, sy) = start.get();
            // Pivot around the compass centre (left square), not the widget.
            let h = f64::from(gesture.widget().map_or(DIAL_BOX, |w| w.height()));
            let c = h / 2.0;
            let px = sx + dx - c;
            let py = sy + dy - c;
            if px == 0.0 && py == 0.0 {
                return;
            }
            // Angle from 12 o'clock, clockwise positive (screen y points down).
            // Snapping to the configured step is applied by the on_rotate handler.
            #[allow(clippy::cast_possible_truncation)]
            let theta = (py.atan2(px) + FRAC_PI_2) as f32;
            on_rotate(theta);
        });
    }
    rotator.add_controller(drag);

    // Right-click (or double-click) resets to 0 deg.
    let reset = gtk::GestureClick::new();
    reset.set_button(gdk::BUTTON_SECONDARY);
    {
        let on_rotate = Rc::clone(on_rotate);
        reset.connect_pressed(move |_, _, _, _| on_rotate(0.0));
    }
    rotator.add_controller(reset);

    let dbl = gtk::GestureClick::new();
    {
        let on_rotate = Rc::clone(on_rotate);
        dbl.connect_pressed(move |_, n_press, _, _| {
            if n_press >= 2 {
                on_rotate(0.0);
            }
        });
    }
    rotator.add_controller(dbl);
}

/// Draw the compass (faint ring + needle pointing in the rotation direction)
/// plus the numeric "X.XX deg" readout, all in the widget's theme colour.
fn draw_rotator(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, _w: i32, h: i32, rotation: f32) {
    let cx = f64::from(h) / 2.0;
    let cy = f64::from(h) / 2.0;
    let r = cy - 1.5;
    if r <= 0.0 {
        return;
    }
    let fg = area.color();
    let (fr, fg_, fb) = (f64::from(fg.red()), f64::from(fg.green()), f64::from(fg.blue()));

    // Ring.
    cr.set_source_rgba(fr, fg_, fb, 0.35);
    cr.set_line_width(1.0);
    cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
    cr.stroke().ok();

    // Needle: up rotated clockwise by `rotation`. up = (0,-1) -> (sin, -cos).
    let theta = f64::from(rotation);
    let (s, c) = theta.sin_cos();
    cr.set_source_rgba(fr, fg_, fb, 0.9);
    cr.set_line_width(1.6);
    cr.move_to(cx, cy);
    cr.line_to(cx + r * s, cy - r * c);
    cr.stroke().ok();
    cr.arc(cx, cy, 1.3, 0.0, std::f64::consts::TAU);
    cr.fill().ok();

    // Numeric readout to the right of the compass.
    let deg = normalize_deg(theta.to_degrees());
    let text = format!("{deg:.2} deg");
    cr.set_font_size(11.0);
    cr.set_source_rgba(fr, fg_, fb, 0.75);
    let ty = cy + cr.text_extents(&text).map_or(4.0, |e| e.height() / 2.0);
    cr.move_to(f64::from(h) + 4.0, ty);
    cr.show_text(&text).ok();
}

/// Normalise degrees to `(-180, 180]` for a tidy readout.
fn normalize_deg(deg: f64) -> f64 {
    #[allow(clippy::cast_possible_truncation)]
    let deg = f64::from(oxiedraw_utils::math::wrap_pi((deg as f32).to_radians()).to_degrees());
    // Avoid printing "-0.00".
    if deg.abs() < 0.005 { 0.0 } else { deg }
}
