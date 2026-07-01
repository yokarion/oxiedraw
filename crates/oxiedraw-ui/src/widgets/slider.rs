use std::cell::Cell;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::prelude::*;

pub(crate) fn build(
    range: (f64, f64),
    step: f64,
    initial: f64,
    width: i32,
    format: impl Fn(f64) -> String + 'static,
    on_change: impl Fn(f64) + 'static,
) -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, range.0, range.1, step);
    scale.set_value(initial);
    scale.set_width_request(width);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_format_value_func(move |_, value| format(value));
    scale.connect_value_changed(move |s| on_change(s.value()));
    install_pen_drag(&scale);
    scale
}

/// Slider whose trough position maps non-linearly to a stepped value.
///
/// The scale runs on a normalized `[0, 1]` position domain; the thumb itself
/// moves freely. `pos_to_value` converts a trough position to the (snapped)
/// value that is reported and displayed, letting different value ranges occupy
/// different fractions of the width. `value_to_pos` only seeds the initial
/// thumb position.
pub(crate) fn build_mapped(
    initial_value: f64,
    width: i32,
    pos_to_value: impl Fn(f64) -> f64 + 'static,
    value_to_pos: impl Fn(f64) -> f64 + 'static,
    format: impl Fn(f64) -> String + 'static,
    on_change: impl Fn(f64) + 'static,
) -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
    scale.set_value(value_to_pos(initial_value));
    scale.set_width_request(width);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    {
        let pos_to_value = Rc::new(pos_to_value);
        {
            let pos_to_value = Rc::clone(&pos_to_value);
            scale.set_format_value_func(move |_, pos| format(pos_to_value(pos)));
        }
        scale.connect_value_changed(move |s| on_change(pos_to_value(s.value())));
    }
    install_pen_drag(&scale);
    scale
}

/// Stylus drags don't drive `GtkRange` internal gesture machinery on
/// GTK4 - tapping the trough registers (so the value can be set) but
/// continuous drags get dropped, leaving the range unmovable with a
/// tablet pen. Attach a dedicated `GestureStylus` that maps stylus
/// position to value via the trough rectangle. The mouse path is left
/// alone so the built-in range gestures keep handling it - duplicating
/// them adds visible lag. Works for both horizontal scales and vertical
/// scrollbars (orientation is read off the range).
pub(crate) fn install_pen_drag(range: &impl IsA<gtk::Range>) {
    let range: gtk::Range = range.upcast_ref::<gtk::Range>().clone();
    let stylus = gtk::GestureStylus::new();
    stylus.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pressed = Rc::new(Cell::new(false));

    {
        let range = range.clone();
        let pressed = Rc::clone(&pressed);
        stylus.connect_down(move |_, x, y| {
            pressed.set(true);
            set_value_from_pointer(&range, x, y);
        });
    }
    {
        let range = range.clone();
        let pressed = Rc::clone(&pressed);
        stylus.connect_motion(move |_, x, y| {
            if pressed.get() {
                set_value_from_pointer(&range, x, y);
            }
        });
    }
    {
        let pressed = Rc::clone(&pressed);
        stylus.connect_up(move |_, _, _| {
            pressed.set(false);
        });
    }

    range.add_controller(stylus);
}

fn set_value_from_pointer(range: &gtk::Range, x: f64, y: f64) {
    let rect = range.range_rect();
    let vertical = range.orientation() == gtk::Orientation::Vertical;
    let (pos, origin, extent) = if vertical {
        (y, f64::from(rect.y()), f64::from(rect.height()))
    } else {
        (x, f64::from(rect.x()), f64::from(rect.width()))
    };
    if extent <= 0.0 {
        return;
    }
    let adjustment = range.adjustment();
    let lower = adjustment.lower();
    // Scrollbars reserve `page_size` for the thumb; scales report 0 here.
    let upper = adjustment.upper() - adjustment.page_size();
    let t = ((pos - origin) / extent).clamp(0.0, 1.0);
    let mapped = lower + t * (upper - lower);
    let value = if range.is_inverted() {
        upper - (mapped - lower)
    } else {
        mapped
    };
    range.set_value(value);
}
