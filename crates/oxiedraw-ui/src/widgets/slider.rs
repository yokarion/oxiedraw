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

/// Stylus drags don't drive `GtkRange` internal gesture machinery on
/// GTK4 - tapping the trough registers (so the value can be set) but
/// continuous drags get dropped, leaving the slider unmovable with a
/// tablet pen. Attach a dedicated `GestureStylus` that maps stylus
/// position to value via the trough rectangle. The mouse path is left
/// alone so the built-in range gestures keep handling it - duplicating
/// them adds visible lag.
fn install_pen_drag(scale: &gtk::Scale) {
    let stylus = gtk::GestureStylus::new();
    stylus.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pressed = Rc::new(Cell::new(false));

    {
        let scale = scale.clone();
        let pressed = Rc::clone(&pressed);
        stylus.connect_down(move |_, x, y| {
            pressed.set(true);
            set_value_from_pointer(&scale, x, y);
        });
    }
    {
        let scale = scale.clone();
        let pressed = Rc::clone(&pressed);
        stylus.connect_motion(move |_, x, y| {
            if pressed.get() {
                set_value_from_pointer(&scale, x, y);
            }
        });
    }
    {
        let pressed = Rc::clone(&pressed);
        stylus.connect_up(move |_, _, _| {
            pressed.set(false);
        });
    }

    scale.add_controller(stylus);
}

fn set_value_from_pointer(scale: &gtk::Scale, x: f64, _y: f64) {
    let rect = scale.range_rect();
    let width = f64::from(rect.width());
    if width <= 0.0 {
        return;
    }
    let adjustment = scale.adjustment();
    let lower = adjustment.lower();
    let upper = adjustment.upper();
    let t = ((x - f64::from(rect.x())) / width).clamp(0.0, 1.0);
    let mapped = lower + t * (upper - lower);
    let value = if scale.is_inverted() {
        upper - (mapped - lower)
    } else {
        mapped
    };
    scale.set_value(value);
}
