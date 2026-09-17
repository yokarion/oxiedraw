use std::rc::Rc;

use oxiedraw_core::curves::{CurveSet, Histogram};
use oxiedraw_core::filters::FilterSpec;
use relm4::gtk::prelude::*;

use super::{FilterContext, affected_layers, open_adjustable};
use crate::widgets::curves_panel;

pub(crate) fn show_curves(ctx: &FilterContext) {
    let identity = FilterSpec::Curves {
        curves: CurveSet::default(),
    };
    open_adjustable(ctx, "Curves", identity, |content, spec, ctx| {
        let histogram = {
            let ctx = ctx.clone();
            move || affected_histogram(&ctx)
        };
        let panel = curves_panel::build(CurveSet::default(), histogram, {
            let spec = Rc::clone(spec);
            let ctx = ctx.clone();
            move |curves| {
                spec.set(FilterSpec::Curves { curves });
                ctx.canvas.borrow_mut().update_filter(spec.get());
                ctx.redraw.request();
            }
        });
        content.append(&panel);
    });
}

fn affected_histogram(ctx: &FilterContext) -> Option<Histogram> {
    let indices: Vec<usize> = affected_layers(ctx).into_iter().map(|(idx, _)| idx).collect();
    ctx.canvas.borrow_mut().layers_histogram(&indices).ok()
}
