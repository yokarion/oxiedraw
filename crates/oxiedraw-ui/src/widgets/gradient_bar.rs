//! Gradient stop bar: a drawn ramp with draggable handles, one per stop.
//! Click to select, click a gap to add a stop, drag to move, drag off the bar
//! to delete. Works on the shared `GradientState`; the panel supplies the
//! selection and stops-changed callbacks.

use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::color::{Color, ColorState};
use oxiedraw_core::tools::GradientState;
use relm4::gtk;
use relm4::gtk::prelude::*;

const WIDGET_HEIGHT: i32 = 56;
const INSET: f64 = 9.0;
const RAMP_H: f64 = 26.0;
const HANDLE_W: f64 = 14.0;
const HANDLE_H: f64 = 20.0;
const HIT_RADIUS: f64 = 13.0;
/// Vertical drag distance past which releasing removes the handle.
const DELETE_DY: f64 = 30.0;
const CHECKER: f64 = 5.0;

pub(crate) struct GradientBar {
    pub(crate) widget: gtk::DrawingArea,
}

impl GradientBar {
    /// Repaint after the ramp changed elsewhere (e.g. a stop recoloured
    /// through the picker).
    pub(crate) fn refresh(&self) {
        self.widget.queue_draw();
    }
}

pub(crate) fn build(
    gradient: &GradientState,
    colors: &ColorState,
    on_selection_changed: Rc<dyn Fn(usize)>,
    on_stops_changed: Rc<dyn Fn()>,
) -> GradientBar {
    let area = gtk::DrawingArea::builder()
        .height_request(WIDGET_HEIGHT)
        .hexpand(true)
        .build();

    {
        let gradient = gradient.clone();
        let colors = colors.clone();
        area.set_draw_func(move |_, cr, w, _| paint(cr, w, &gradient, &colors));
    }

    install_input(
        &area,
        gradient.clone(),
        colors.clone(),
        on_selection_changed,
        on_stops_changed,
    );

    GradientBar { widget: area }
}

fn ramp_width(width: i32) -> f64 {
    (f64::from(width) - 2.0 * INSET).max(1.0)
}

fn t_from_x(x: f64, width: i32) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    let t = ((x - INSET) / ramp_width(width)).clamp(0.0, 1.0) as f32;
    t
}

fn x_from_t(t: f32, width: i32) -> f64 {
    INSET + f64::from(t) * ramp_width(width)
}

fn paint(cr: &gtk::cairo::Context, width: i32, gradient: &GradientState, colors: &ColorState) {
    let settings = gradient.resolve(colors);
    let selected = gradient.selected_stop.get();
    let rw = ramp_width(width);

    // Checkerboard behind the ramp so transparent stops read.
    draw_checker(cr, INSET, INSET, rw, RAMP_H);

    // Ramp: many vertical strips sampled from the settings.
    let strips = rw.ceil() as i32;
    for i in 0..strips {
        let t = f64::from(i) / f64::from((strips - 1).max(1));
        #[allow(clippy::cast_possible_truncation)]
        let (color, opacity) = settings.sample_srgb(t as f32);
        cr.rectangle(INSET + f64::from(i), INSET, 2.0, RAMP_H);
        set_rgba(cr, color, opacity);
        cr.fill().ok();
    }

    // Ramp border.
    cr.rectangle(INSET, INSET, rw, RAMP_H);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.4);
    cr.set_line_width(1.0);
    cr.stroke().ok();

    // Handles.
    for (i, stop) in settings.stops.iter().enumerate() {
        draw_handle(cr, x_from_t(stop.position, width), stop.color, stop.opacity, i == selected);
    }
}

fn draw_checker(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64) {
    cr.save().ok();
    cr.rectangle(x, y, w, h);
    cr.clip();
    cr.rectangle(x, y, w, h);
    cr.set_source_rgb(0.6, 0.6, 0.6);
    cr.fill().ok();
    let cols = (w / CHECKER).ceil() as i32;
    let rows = (h / CHECKER).ceil() as i32;
    cr.set_source_rgb(0.85, 0.85, 0.85);
    for row in 0..rows {
        for col in 0..cols {
            if (row + col) % 2 == 0 {
                cr.rectangle(x + f64::from(col) * CHECKER, y + f64::from(row) * CHECKER, CHECKER, CHECKER);
                cr.fill().ok();
            }
        }
    }
    cr.restore().ok();
}

/// A pin handle pointing up into the ramp, its body filled with the stop
/// colour over a checkerboard so partial opacity shows.
fn draw_handle(cr: &gtk::cairo::Context, cx: f64, color: Color, opacity: f32, selected: bool) {
    let apex_y = INSET + RAMP_H - 1.0;
    let top_y = apex_y + 5.0;
    let bottom_y = apex_y + HANDLE_H;
    let half = HANDLE_W / 2.0;

    let trace = |cr: &gtk::cairo::Context| {
        cr.move_to(cx, apex_y);
        cr.line_to(cx + half, top_y);
        cr.line_to(cx + half, bottom_y);
        cr.line_to(cx - half, bottom_y);
        cr.line_to(cx - half, top_y);
        cr.close_path();
    };

    // Checkerboard body then the colour at its alpha.
    draw_checker(cr, cx - half, top_y, HANDLE_W, bottom_y - top_y);
    trace(cr);
    set_rgba(cr, color, opacity);
    cr.fill().ok();

    // Outline: accent-ish white for the selected handle, dark otherwise.
    trace(cr);
    if selected {
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.set_line_width(2.0);
    } else {
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.7);
        cr.set_line_width(1.0);
    }
    cr.stroke().ok();
}

fn set_rgba(cr: &gtk::cairo::Context, color: Color, opacity: f32) {
    cr.set_source_rgba(
        f64::from(color.r) / 255.0,
        f64::from(color.g) / 255.0,
        f64::from(color.b) / 255.0,
        f64::from(opacity.clamp(0.0, 1.0)),
    );
}

/// Nearest stop whose handle is within `HIT_RADIUS` px of `x`, if any.
fn hit_stop(gradient: &GradientState, colors: &ColorState, width: i32, x: f64) -> Option<usize> {
    let settings = gradient.resolve(colors);
    let mut best: Option<(usize, f64)> = None;
    for (i, stop) in settings.stops.iter().enumerate() {
        let d = (x_from_t(stop.position, width) - x).abs();
        if d <= HIT_RADIUS && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((i, d));
        }
    }
    best.map(|(i, _)| i)
}

fn install_input(
    area: &gtk::DrawingArea,
    gradient: GradientState,
    colors: ColorState,
    on_selection_changed: Rc<dyn Fn(usize)>,
    on_stops_changed: Rc<dyn Fn()>,
) {
    // Handle grabbed by the current drag; delete armed once dragged off the bar.
    let grabbed: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
    let pending_delete = Rc::new(Cell::new(false));

    let on_press: Rc<dyn Fn(f64, f64)> = {
        let area = area.clone();
        let gradient = gradient.clone();
        let colors = colors.clone();
        let grabbed = Rc::clone(&grabbed);
        let pending_delete = Rc::clone(&pending_delete);
        let on_selection_changed = Rc::clone(&on_selection_changed);
        let on_stops_changed = Rc::clone(&on_stops_changed);
        Rc::new(move |x: f64, _y: f64| {
            let width = area.width();
            gradient.ensure_owned(&colors);
            pending_delete.set(false);

            let idx = if let Some(hit) = hit_stop(&gradient, &colors, width, x) {
                hit
            } else {
                // Empty spot: insert a stop interpolated at this position.
                let t = t_from_x(x, width);
                let new_idx = gradient
                    .settings
                    .borrow_mut()
                    .as_mut()
                    .map_or(0, |s| s.insert_stop(t));
                on_stops_changed();
                new_idx
            };
            grabbed.set(Some(idx));
            gradient.selected_stop.set(idx);
            on_selection_changed(idx);
            area.queue_draw();
        })
    };

    let on_move: Rc<dyn Fn(f64, f64)> = {
        let area = area.clone();
        let gradient = gradient.clone();
        let grabbed = Rc::clone(&grabbed);
        let pending_delete = Rc::clone(&pending_delete);
        let on_selection_changed = Rc::clone(&on_selection_changed);
        let on_stops_changed = Rc::clone(&on_stops_changed);
        Rc::new(move |x: f64, dy: f64| {
            let Some(gi) = grabbed.get() else {
                return;
            };
            let width = area.width();

            // Dragged far off the bar arms a delete; snap back to move otherwise.
            let can_delete = gradient
                .settings
                .borrow()
                .as_ref()
                .is_some_and(|s| s.stops.len() > 2);
            if can_delete && dy.abs() > DELETE_DY {
                pending_delete.set(true);
                area.queue_draw();
                return;
            }
            pending_delete.set(false);

            let t = t_from_x(x, width);
            let new_idx = gradient
                .settings
                .borrow_mut()
                .as_mut()
                .map_or(gi, |s| s.move_stop(gi, t));
            grabbed.set(Some(new_idx));
            gradient.selected_stop.set(new_idx);
            on_selection_changed(new_idx);
            on_stops_changed();
            area.queue_draw();
        })
    };

    let on_release: Rc<dyn Fn()> = {
        let area = area.clone();
        let gradient = gradient.clone();
        let grabbed = Rc::clone(&grabbed);
        let pending_delete = Rc::clone(&pending_delete);
        let on_selection_changed = Rc::clone(&on_selection_changed);
        let on_stops_changed = Rc::clone(&on_stops_changed);
        Rc::new(move || {
            if pending_delete.get()
                && let Some(gi) = grabbed.get()
            {
                let removed = gradient
                    .settings
                    .borrow_mut()
                    .as_mut()
                    .is_some_and(|s| s.remove_stop(gi));
                if removed {
                    let len = gradient
                        .settings
                        .borrow()
                        .as_ref()
                        .map_or(1, |s| s.stops.len());
                    let sel = gi.min(len - 1);
                    gradient.selected_stop.set(sel);
                    on_selection_changed(sel);
                    on_stops_changed();
                }
            }
            grabbed.set(None);
            pending_delete.set(false);
            area.queue_draw();
        })
    };

    // Mouse / touch drag.
    let drag = gtk::GestureDrag::new();
    {
        let on_press = Rc::clone(&on_press);
        drag.connect_drag_begin(move |_, x, y| on_press(x, y));
    }
    {
        let on_move = Rc::clone(&on_move);
        drag.connect_drag_update(move |g, ox, oy| {
            if let Some((sx, _)) = g.start_point() {
                on_move(sx + ox, oy);
            }
        });
    }
    {
        let on_release = Rc::clone(&on_release);
        drag.connect_drag_end(move |_, _, _| on_release());
    }
    area.add_controller(drag);

    // GestureDrag drops continuous pen drags on GTK4, so map the pen directly.
    let stylus = gtk::GestureStylus::new();
    stylus.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pen_start = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    {
        let on_press = Rc::clone(&on_press);
        let pen_start = Rc::clone(&pen_start);
        stylus.connect_down(move |_, x, y| {
            pen_start.set((x, y));
            on_press(x, y);
        });
    }
    {
        let on_move = Rc::clone(&on_move);
        let pen_start = Rc::clone(&pen_start);
        stylus.connect_motion(move |_, x, y| {
            let (_, sy) = pen_start.get();
            on_move(x, y - sy);
        });
    }
    {
        let on_release = Rc::clone(&on_release);
        stylus.connect_up(move |_, _, _| on_release());
    }
    area.add_controller(stylus);
}
