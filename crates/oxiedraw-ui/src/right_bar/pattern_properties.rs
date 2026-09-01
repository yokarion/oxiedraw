//! The Pattern tool's panel: which generator, and its knobs.
//!
//! Built entirely from the generator's schema, so a pattern publishing a
//! `&'static [ParamDef]` gets a working panel with no code in this file. Only
//! the knobs a schema marks `exposed` are offered; the rest would bury the
//! handful worth reaching for.

use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::patterns::PatternState;
use oxiedraw_patterns::{ParamDef, ParamKind, ParamValue, Params};
use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::widgets::boxed_list;

/// Width of the value readout beside each slider, so the rows line up.
const VALUE_WIDTH: i32 = 52;

/// Build the panel. Nothing outside this file refreshes it: each control is
/// constructed from the stored value and is the only thing that writes it back,
/// and the two things that change the whole set rebuild from in here.
pub(crate) fn build(pattern: &PatternState, on_change: Rc<dyn Fn()>) -> gtk::Widget {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    // Driven by the registry, so new generators appear here as they land.
    let ids: Vec<&'static str> = oxiedraw_patterns::PATTERNS.iter().map(|p| p.id()).collect();
    let labels: Vec<&str> = oxiedraw_patterns::PATTERNS.iter().map(|p| p.label()).collect();
    let chooser = gtk::DropDown::from_strings(&labels);
    let selected = ids
        .iter()
        .position(|id| *id == pattern.pattern().id())
        .unwrap_or(0);
    chooser.set_selected(selected as u32);

    let chooser_list = boxed_list::list();
    chooser_list.append(&boxed_list::row("Pattern", &chooser, &[]));
    root.append(&chooser_list);

    // Their own box, so switching pattern can replace them wholesale without
    // disturbing the chooser above.
    let knobs = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    root.append(&knobs);

    let rebuild: Rc<dyn Fn()> = {
        let knobs = knobs.clone();
        let pattern = pattern.clone();
        let on_change = Rc::clone(&on_change);
        Rc::new(move || {
            while let Some(child) = knobs.first_child() {
                knobs.remove(&child);
            }
            let list = boxed_list::list();
            let mut any = false;
            for def in pattern.pattern().schema().iter().filter(|d| d.exposed) {
                list.append(&control_for(def, &pattern, &on_change));
                any = true;
            }
            if any {
                knobs.append(&list);
            }
        })
    };
    rebuild();

    // Rebuilding is what moves the controls, since they are constructed from
    // the stored values.
    let reset = gtk::Button::builder()
        .label("Reset to defaults")
        .halign(gtk::Align::End)
        .build();
    reset.add_css_class("flat");
    root.append(&reset);
    {
        let pattern = pattern.clone();
        let rebuild = Rc::clone(&rebuild);
        let on_change = Rc::clone(&on_change);
        reset.connect_clicked(move |_| {
            pattern.set_params(pattern.pattern().defaults());
            rebuild();
            on_change();
        });
    }

    {
        let pattern = pattern.clone();
        let rebuild = Rc::clone(&rebuild);
        let on_change = Rc::clone(&on_change);
        chooser.connect_selected_notify(move |chooser| {
            let Some(id) = ids.get(chooser.selected() as usize) else {
                return;
            };
            (*id).clone_into(&mut pattern.pattern_id.borrow_mut());
            rebuild();
            on_change();
        });
    }

    root.upcast()
}

fn control_for(
    def: &'static ParamDef,
    pattern: &PatternState,
    on_change: &Rc<dyn Fn()>,
) -> gtk::ListBoxRow {
    let row = match def.kind {
        ParamKind::Float {
            min, max, step, ..
        } => {
            let value = f64::from(read_float(pattern, def.id));
            slider_row(
                def,
                min.into(),
                max.into(),
                step.into(),
                value,
                pattern,
                on_change,
                move |params, id, v| params.set_float(id, v as f32),
            )
        }
        ParamKind::Int { min, max, .. } => {
            let value = f64::from(read_int(pattern, def.id));
            slider_row(
                def,
                min.into(),
                max.into(),
                1.0,
                value,
                pattern,
                on_change,
                move |params, id, v| params.set_int(id, v.round() as i32),
            )
        }
        ParamKind::Bool { .. } => {
            let toggle = gtk::Switch::builder()
                .valign(gtk::Align::Center)
                .halign(gtk::Align::End)
                .hexpand(true)
                .active(read_bool(pattern, def.id))
                .build();
            let pattern = pattern.clone();
            let on_change = Rc::clone(on_change);
            toggle.connect_active_notify(move |toggle| {
                let mut params = pattern.params();
                params.set_bool(def.id, toggle.is_active());
                pattern.set_params(params);
                on_change();
            });
            boxed_list::row(def.label, &toggle, &[])
        }
        ParamKind::Choice { options, .. } => {
            let dropdown = gtk::DropDown::from_strings(options);
            dropdown.set_selected(read_choice(pattern, def.id));
            let pattern = pattern.clone();
            let on_change = Rc::clone(on_change);
            dropdown.connect_selected_notify(move |dropdown| {
                let mut params = pattern.params();
                params.set_choice(def.id, dropdown.selected());
                pattern.set_params(params);
                on_change();
            });
            boxed_list::row(def.label, &dropdown, &[])
        }
        ParamKind::FloatRange {
            min, max, step, ..
        } => range_row(
            def,
            min.into(),
            max.into(),
            step.into(),
            pattern,
            on_change,
            move |params, id, v| params.set_float(id, v as f32),
            |pattern, id| f64::from(read_float(pattern, id)),
        ),
        ParamKind::IntRange { min, max, .. } => range_row(
            def,
            min.into(),
            max.into(),
            1.0,
            pattern,
            on_change,
            move |params, id, v| params.set_int(id, v.round() as i32),
            |pattern, id| f64::from(read_int(pattern, id)),
        ),
    };
    row.set_tooltip_text(Some(def.tooltip));
    row
}

#[allow(clippy::too_many_arguments)]
fn slider_row(
    def: &'static ParamDef,
    min: f64,
    max: f64,
    step: f64,
    value: f64,
    pattern: &PatternState,
    on_change: &Rc<dyn Fn()>,
    store: impl Fn(&mut Params, &str, f64) + 'static,
) -> gtk::ListBoxRow {
    let (scale, readout, holder) = slider(min, max, step, value);
    let pattern = pattern.clone();
    let on_change = Rc::clone(on_change);
    on_scale_changed(&scale, min, max, move |value| {
        readout.set_label(&format_value(value, step));
        let mut params = pattern.params();
        store(&mut params, def.id, value);
        pattern.set_params(params);
        on_change();
    });
    boxed_list::row(def.label, &holder, &[])
}

/// Both ends of a span on one row, so it reads as one control.
#[allow(clippy::too_many_arguments)]
fn range_row(
    def: &'static ParamDef,
    min: f64,
    max: f64,
    step: f64,
    pattern: &PatternState,
    on_change: &Rc<dyn Fn()>,
    store: impl Fn(&mut Params, &str, f64) + Clone + 'static,
    read: impl Fn(&PatternState, &str) -> f64,
) -> gtk::ListBoxRow {
    let ends = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .build();
    for part in ["lo", "hi"] {
        let id = format!("{}.{part}", def.id);
        let (scale, readout, holder) = slider(min, max, step, read(pattern, &id));
        let pattern = pattern.clone();
        let on_change = Rc::clone(on_change);
        let store = store.clone();
        on_scale_changed(&scale, min, max, move |value| {
            readout.set_label(&format_value(value, step));
            let mut params = pattern.params();
            store(&mut params, &id, value);
            pattern.set_params(params);
            on_change();
        });
        ends.append(&holder);
    }
    boxed_list::row(def.label, &ends, &[])
}

/// How close to a notable value counts as close, as a fraction of the range.
/// Small enough that a value deliberately set just off one still lands where it
/// was put.
const SNAP_FRACTION: f64 = 0.02;

/// The values a slider settles onto when you come near: for a knob that runs
/// either side of zero, both ends and the neutral middle. Only those - snapping
/// the end of a plain 2..400 range would put ordinary values out of reach.
fn snap(value: f64, min: f64, max: f64) -> f64 {
    if min >= 0.0 || max <= 0.0 {
        return value;
    }
    let tolerance = (max - min) * SNAP_FRACTION;
    [min, 0.0, max]
        .into_iter()
        .find(|target| (value - target).abs() <= tolerance)
        .unwrap_or(value)
}

fn on_scale_changed(scale: &gtk::Scale, min: f64, max: f64, on_value: impl Fn(f64) + 'static) {
    // Writing the snapped value back re-enters this handler; the guard makes
    // that pass a no-op. The snapped value is a fixed point, so it cannot loop.
    let adjusting = Rc::new(Cell::new(false));
    scale.connect_value_changed(move |scale| {
        if adjusting.get() {
            return;
        }
        let raw = scale.value();
        let value = snap(raw, min, max);
        if (value - raw).abs() > f64::EPSILON {
            adjusting.set(true);
            scale.set_value(value);
            adjusting.set(false);
        }
        on_value(value);
    });
}

fn slider(min: f64, max: f64, step: f64, value: f64) -> (gtk::Scale, gtk::Label, gtk::Box) {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step.max(0.0001));
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.set_value(value);
    // GtkRange's own gestures drop continuous stylus drags, so a pen can tap a
    // slider but not drag it. Every scale in the app needs this.
    crate::widgets::slider::install_pen_drag(&scale);
    let readout = gtk::Label::builder()
        .label(format_value(value, step))
        .xalign(1.0)
        .width_request(VALUE_WIDTH)
        .build();
    readout.add_css_class("dim-label");
    let holder = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .hexpand(true)
        .build();
    holder.append(&scale);
    holder.append(&readout);
    (scale, readout, holder)
}

/// Decimals to match the step, so a 0.01 knob does not read as "0" and a whole
/// number does not read as "44.00".
fn format_value(value: f64, step: f64) -> String {
    if step >= 1.0 {
        format!("{value:.0}")
    } else if step >= 0.1 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn read_float(pattern: &PatternState, id: &str) -> f32 {
    let params = pattern.params();
    match params.get(id) {
        Some(ParamValue::Float(v)) => v,
        #[allow(clippy::cast_precision_loss)]
        Some(ParamValue::Int(v)) => v as f32,
        _ => pattern.resolved(&params).float(id),
    }
}

fn read_int(pattern: &PatternState, id: &str) -> i32 {
    let params = pattern.params();
    match params.get(id) {
        Some(ParamValue::Int(v)) => v,
        #[allow(clippy::cast_possible_truncation)]
        Some(ParamValue::Float(v)) => v.round() as i32,
        _ => pattern.resolved(&params).int(id),
    }
}

fn read_bool(pattern: &PatternState, id: &str) -> bool {
    let params = pattern.params();
    pattern.resolved(&params).bool(id)
}

fn read_choice(pattern: &PatternState, id: &str) -> u32 {
    let params = pattern.params();
    pattern.resolved(&params).choice(id)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    // Everything else is left where it was put: too wide a pull makes the
    // values just off a snap point unreachable.
    #[test]
    fn a_bipolar_knob_settles_on_its_ends_and_its_middle() {
        for (raw, want) in [
            (-0.99, -1.0),
            (-1.0, -1.0),
            (0.015, 0.0),
            (-0.02, 0.0),
            (0.98, 1.0),
            // Outside the pull: left exactly where it landed.
            (-0.5, -0.5),
            (0.25, 0.25),
            (0.9, 0.9),
        ] {
            assert_eq!(snap(raw, -1.0, 1.0), want, "snapping {raw}");
        }
    }

    #[test]
    fn a_one_sided_range_does_not_snap() {
        assert_eq!(snap(2.5, 2.0, 400.0), 2.5);
        assert_eq!(snap(399.0, 2.0, 400.0), 399.0);
        assert_eq!(snap(0.004, 0.0, 1.0), 0.004);
    }

    // Snapping has to be idempotent, or writing the snapped value back into the
    // slider would move it again and the handler would never settle.
    #[test]
    fn a_snapped_value_snaps_to_itself() {
        for target in [-1.0, 0.0, 1.0] {
            assert_eq!(snap(snap(target, -1.0, 1.0), -1.0, 1.0), target);
        }
    }
}
