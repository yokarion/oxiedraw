//! Krita-style adjustment slider: a custom-drawn gradient bar with a marker
//! plus a native (libadwaita-themed) spin button for the numeric value.
//!
//! The gradient is painted with cairo so it can show any color ramp the
//! caller supplies (sampled across the bar), with rounded corners and a
//! subtle border. The numeric input stays a stock `GtkSpinButton` so it
//! inherits the platform theme. Dragging the bar (mouse, touch, or stylus)
//! and editing the spin button stay in sync and both fire `on_change`.

use std::cell::Cell;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::prelude::*;

const LABEL_WIDTH: i32 = 80;
const BAR_HEIGHT: i32 = 30;
const MARKER_HEIGHT: f64 = 7.0;
const MARKER_HALF_WIDTH: f64 = 5.0;
const CORNER_RADIUS: f64 = 4.0;
const BORDER_INSET: f64 = 0.5;
const GRADIENT_STOPS: usize = 64;

/// A built gradient slider. Append [`Self::widget`] to a container; read the
/// current value with [`Self::value`]; call [`Self::refresh`] to repaint the
/// gradient after the ramp it depends on has changed.
#[derive(Clone)]
pub(crate) struct GradientSlider {
    pub widget: gtk::Box,
    area: gtk::DrawingArea,
    spin: gtk::SpinButton,
}

impl GradientSlider {
    /// The gradient `DrawingArea`. Clone it and call `queue_draw()` to
    /// repaint when a ramp this slider depends on has changed elsewhere.
    pub(crate) fn area(&self) -> gtk::DrawingArea {
        self.area.clone()
    }

    /// Run `f` with the new value on every change (bar drag or spin edit).
    pub(crate) fn connect_changed(&self, f: impl Fn(f64) + 'static) {
        self.spin.connect_value_changed(move |s| f(s.value()));
    }

    /// Set the value programmatically (e.g. an external refresh). This still
    /// fires the spin's `value-changed`, so callers that want to avoid a
    /// feedback loop should guard their `on_change` with their own sync flag.
    pub(crate) fn set_value(&self, v: f64) {
        self.spin.set_value(v);
    }

    /// Hide the numeric spin button, leaving just the gradient bar. Used where
    /// the exact number is meaningless (e.g. a colour picker). The spin stays
    /// live under the hood, so bar drags still drive `on_change`.
    pub(crate) fn hide_spin(&self) {
        self.spin.set_visible(false);
    }
}

/// Build a labeled gradient slider.
///
/// `gradient` maps a position `t` in `0.0..=1.0` along the bar to an RGB
/// color (each channel `0.0..=1.0`). `on_change` fires on every value change
/// from either the bar or the spin button.
pub(crate) fn build(
    label: &str,
    range: (f64, f64),
    step: f64,
    digits: u32,
    initial: f64,
    gradient: impl Fn(f64) -> (f64, f64, f64) + 'static,
    on_change: impl Fn(f64) + 'static,
) -> GradientSlider {
    let (min, max) = range;
    let on_change: Rc<dyn Fn(f64)> = Rc::new(on_change);
    let gradient: Rc<dyn Fn(f64) -> (f64, f64, f64)> = Rc::new(gradient);

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    let lbl = gtk::Label::builder()
        .label(label)
        .use_underline(true)
        .xalign(0.0)
        .width_request(LABEL_WIDTH)
        .build();

    let area = gtk::DrawingArea::builder()
        .height_request(BAR_HEIGHT)
        .hexpand(true)
        .build();

    let spin = gtk::SpinButton::with_range(min, max, step);
    spin.set_digits(digits);
    spin.set_value(initial);
    lbl.set_mnemonic_widget(Some(&spin));

    // Re-entrancy guard: programmatic spin/value updates would otherwise
    // bounce between the bar drag and the spin's value-changed handler.
    let syncing = Rc::new(Cell::new(false));
    let current = Rc::new(Cell::new(initial));

    let set_value: Rc<dyn Fn(f64)> = {
        let spin = spin.clone();
        let area = area.clone();
        let on_change = Rc::clone(&on_change);
        let syncing = Rc::clone(&syncing);
        let current = Rc::clone(&current);
        Rc::new(move |v: f64| {
            let v = v.clamp(min, max);
            current.set(v);
            syncing.set(true);
            spin.set_value(v);
            syncing.set(false);
            area.queue_draw();
            on_change(v);
        })
    };

    {
        let syncing = Rc::clone(&syncing);
        let current = Rc::clone(&current);
        let area = area.clone();
        let on_change = Rc::clone(&on_change);
        spin.connect_value_changed(move |s| {
            if syncing.get() {
                return;
            }
            let v = s.value();
            current.set(v);
            area.queue_draw();
            on_change(v);
        });
    }

    install_bar_input(&area, range, &current, &set_value);

    // Repaint the marker against the live value whenever the bar redraws.
    {
        let current = Rc::clone(&current);
        let gradient = Rc::clone(&gradient);
        area.set_draw_func(move |a, cr, w, h| {
            paint(a, cr, w, h, range, current.get(), gradient.as_ref());
        });
    }

    row.append(&lbl);
    row.append(&area);
    row.append(&spin);

    GradientSlider { widget: row, area, spin }
}

/// HSL (`h` in degrees, `s`/`l` in `0.0..=1.0`) to RGB (`0.0..=1.0`).
/// Handy for building gradient ramps for the hue/saturation/lightness bars.
pub(crate) fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    if s <= 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let h = h.rem_euclid(360.0) / 360.0;
    (
        hue_to_channel(p, q, h + 1.0 / 3.0),
        hue_to_channel(p, q, h),
        hue_to_channel(p, q, h - 1.0 / 3.0),
    )
}

fn hue_to_channel(p: f64, q: f64, t: f64) -> f64 {
    let t = t.rem_euclid(1.0);
    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 1.0 / 2.0 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

fn bar_geometry(width: i32, height: i32) -> (f64, f64, f64, f64) {
    let bar_w = f64::from(width) - 2.0 * BORDER_INSET;
    let bar_h = f64::from(height) - MARKER_HEIGHT - BORDER_INSET;
    (BORDER_INSET, BORDER_INSET, bar_w.max(0.0), bar_h.max(0.0))
}

fn paint(
    _area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    range: (f64, f64),
    value: f64,
    gradient: &dyn Fn(f64) -> (f64, f64, f64),
) {
    let (x, y, w, h) = bar_geometry(width, height);
    if w <= 0.0 || h <= 0.0 {
        return;
    }

    // Gradient fill, clipped to the rounded bar.
    rounded_rect(cr, x, y, w, h, CORNER_RADIUS);
    cr.clip_preserve();
    let ramp = gtk::cairo::LinearGradient::new(x, y, x + w, y);
    for i in 0..=GRADIENT_STOPS {
        let t = i as f64 / GRADIENT_STOPS as f64;
        let (r, g, b) = gradient(t);
        ramp.add_color_stop_rgb(t, r, g, b);
    }
    let _ = cr.set_source(&ramp);
    let _ = cr.fill_preserve();
    cr.reset_clip();

    // Subtle border.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.35);
    cr.set_line_width(1.0);
    let _ = cr.stroke();

    // Upward-pointing marker below the bar at the value position.
    let t = if (range.1 - range.0).abs() < f64::EPSILON {
        0.0
    } else {
        (value - range.0) / (range.1 - range.0)
    };
    let mx = x + t.clamp(0.0, 1.0) * w;
    let apex_y = y + h;
    let base_y = apex_y + MARKER_HEIGHT;
    cr.move_to(mx, apex_y);
    cr.line_to(mx - MARKER_HALF_WIDTH, base_y);
    cr.line_to(mx + MARKER_HALF_WIDTH, base_y);
    cr.close_path();
    cr.set_source_rgb(0.85, 0.85, 0.85);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    let deg = std::f64::consts::PI / 180.0;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -90.0 * deg, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, 90.0 * deg);
    cr.arc(x + r, y + h - r, r, 90.0 * deg, 180.0 * deg);
    cr.arc(x + r, y + r, r, 180.0 * deg, 270.0 * deg);
    cr.close_path();
}

fn install_bar_input(
    area: &gtk::DrawingArea,
    range: (f64, f64),
    current: &Rc<Cell<f64>>,
    set_value: &Rc<dyn Fn(f64)>,
) {
    let value_from_x = {
        let area = area.clone();
        move |x: f64| -> f64 {
            let (bx, _, bw, _) = bar_geometry(area.width(), area.height());
            if bw <= 0.0 {
                return range.0;
            }
            let t = ((x - bx) / bw).clamp(0.0, 1.0);
            range.0 + t * (range.1 - range.0)
        }
    };

    // Mouse / touch drag.
    let drag = gtk::GestureDrag::new();
    {
        let set_value = Rc::clone(set_value);
        let value_from_x = value_from_x.clone();
        drag.connect_drag_begin(move |_, x, _| set_value(value_from_x(x)));
    }
    {
        let set_value = Rc::clone(set_value);
        let value_from_x = value_from_x.clone();
        drag.connect_drag_update(move |g, ox, _| {
            if let Some((sx, _)) = g.start_point() {
                set_value(value_from_x(sx + ox));
            }
        });
    }
    area.add_controller(drag);

    // Stylus: GtkRange-style gestures drop continuous pen drags on GTK4, so
    // map the pen position directly like the plain slider does.
    let stylus = gtk::GestureStylus::new();
    stylus.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pen_down = Rc::new(Cell::new(false));
    {
        let set_value = Rc::clone(set_value);
        let value_from_x = value_from_x.clone();
        let pen_down = Rc::clone(&pen_down);
        stylus.connect_down(move |_, x, _| {
            pen_down.set(true);
            set_value(value_from_x(x));
        });
    }
    {
        let set_value = Rc::clone(set_value);
        let value_from_x = value_from_x.clone();
        let pen_down = Rc::clone(&pen_down);
        stylus.connect_motion(move |_, x, _| {
            if pen_down.get() {
                set_value(value_from_x(x));
            }
        });
    }
    {
        let pen_down = Rc::clone(&pen_down);
        stylus.connect_up(move |_, _, _| pen_down.set(false));
    }
    area.add_controller(stylus);

    // Scroll wheel nudges by one step.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    {
        let set_value = Rc::clone(set_value);
        let current = Rc::clone(current);
        scroll.connect_scroll(move |_, _, dy| {
            let step = (range.1 - range.0).signum() * if dy > 0.0 { -1.0 } else { 1.0 };
            set_value(current.get() + step);
            gtk::glib::Propagation::Stop
        });
    }
    area.add_controller(scroll);
}
