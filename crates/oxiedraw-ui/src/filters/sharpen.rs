use std::rc::Rc;

use oxiedraw_core::filters::FilterSpec;
use relm4::gtk::prelude::*;

use super::{filter_type_row, labeled_slider, open_adjustable, FilterContext};

pub(crate) fn show_sharpen(ctx: &FilterContext) {
    open_adjustable(
        ctx,
        "Sharpen",
        FilterSpec::Sharpen { amount: 0.0 },
        |content, spec, ctx| {
            let push = {
                let spec = Rc::clone(spec);
                let ctx = ctx.clone();
                move || {
                    ctx.canvas.borrow_mut().update_filter(spec.get());
                    ctx.redraw.request();
                }
            };
            content.append(&filter_type_row("Type", &["Unsharp Mask"]));
            content.append(&labeled_slider("Strength", (0.0, 50.0), 0.05, 0.0, {
                let spec = Rc::clone(spec);
                move |v| {
                    spec.set(FilterSpec::Sharpen { amount: v as f32 });
                    push();
                }
            }));
        },
    );
}
