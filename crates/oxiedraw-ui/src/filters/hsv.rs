use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::filters::FilterSpec;
use relm4::gtk::prelude::*;

use super::{open_adjustable, FilterContext};
use crate::widgets::gradient_slider::{self, hsl_to_rgb};

// Krita-style ranges. Saturation and lightness are mapped to the core's
// multiplier params (identity 1.0) as `1 + n/100`; hue maps straight to
// degrees of rotation.
const HUE_RANGE: (f64, f64) = (-180.0, 180.0);
const SAT_RANGE: (f64, f64) = (-100.0, 100.0);
const LIGHT_RANGE: (f64, f64) = (-100.0, 100.0);

pub(crate) fn show_hsv(ctx: &FilterContext) {
    open_adjustable(
        ctx,
        "Hue / Saturation / Lightness",
        FilterSpec::hsv_identity(),
        |content, spec, ctx| {
            let push = {
                let spec = Rc::clone(spec);
                let ctx = ctx.clone();
                move || {
                    ctx.canvas.borrow_mut().update_filter(spec.get());
                    ctx.redraw.request();
                }
            };

            // The hue ramp shifts with the rotation value, so every slider
            // shares this cell and the hue bar is refreshed when it moves.
            let hue = Rc::new(Cell::new(0.0_f64));

            // Hue: full spectrum, offset by the current rotation.
            let hue_slider = gradient_slider::build(
                "_Hue",
                HUE_RANGE,
                1.0,
                0,
                0.0,
                {
                    let hue = Rc::clone(&hue);
                    move |t| hsl_to_rgb(t * 360.0 + hue.get(), 1.0, 0.5)
                },
                {
                    let spec = Rc::clone(spec);
                    let push = push.clone();
                    let hue = Rc::clone(&hue);
                    move |v| {
                        hue.set(v);
                        update_hsv(&spec, |h| h.hue_degrees = v as f32);
                        push();
                    }
                },
            );

            // Saturation: gray to a vivid hue at mid lightness.
            let sat_slider = gradient_slider::build(
                "_Saturation",
                SAT_RANGE,
                1.0,
                0,
                0.0,
                {
                    let hue = Rc::clone(&hue);
                    move |t| hsl_to_rgb(hue.get(), t, 0.5)
                },
                {
                    let spec = Rc::clone(spec);
                    let push = push.clone();
                    move |v| {
                        update_hsv(&spec, |h| h.saturation = (1.0 + v / 100.0) as f32);
                        push();
                    }
                },
            );

            // Lightness: black through gray to white.
            let light_slider = gradient_slider::build(
                "_Lightness",
                LIGHT_RANGE,
                1.0,
                0,
                0.0,
                |t| (t, t, t),
                {
                    let spec = Rc::clone(spec);
                    let push = push.clone();
                    move |v| {
                        update_hsv(&spec, |h| h.value = (1.0 + v / 100.0) as f32);
                        push();
                    }
                },
            );

            // Moving hue also restyles the saturation ramp (it samples the
            // current hue), so repaint that bar on every hue change.
            let sat_area = sat_slider.area();
            hue_slider.connect_changed(move |_| sat_area.queue_draw());

            content.append(&hue_slider.widget);
            content.append(&sat_slider.widget);
            content.append(&light_slider.widget);
        },
    );
}

fn update_hsv(spec: &Rc<Cell<FilterSpec>>, f: impl FnOnce(&mut HsvFields)) {
    if let FilterSpec::Hsv {
        hue_degrees,
        saturation,
        value,
    } = spec.get()
    {
        let mut fields = HsvFields {
            hue_degrees,
            saturation,
            value,
        };
        f(&mut fields);
        spec.set(FilterSpec::Hsv {
            hue_degrees: fields.hue_degrees,
            saturation: fields.saturation,
            value: fields.value,
        });
    }
}

struct HsvFields {
    hue_degrees: f32,
    saturation: f32,
    value: f32,
}
