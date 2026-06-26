use std::rc::Rc;

use oxiedraw_core::filters::FilterSpec;
use relm4::gtk;
use relm4::gtk::prelude::*;

use super::{FilterContext, open_adjustable};
use crate::widgets::{boxed_list, slider};

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

            let list = boxed_list::list();

            let type_combo = gtk::DropDown::from_strings(&["Unsharp Mask"]);
            list.append(&boxed_list::row("Type", &type_combo, &[]));

            let strength = slider::build((0.0, 100.0), 0.05, 0.0, 200, |v| format!("{v:.2}"), {
                let spec = Rc::clone(spec);
                move |v| {
                    spec.set(FilterSpec::Sharpen { amount: v as f32 });
                    push();
                }
            });
            list.append(&boxed_list::row("Strength", &strength, &[]));
            content.append(&list);
        },
    );
}
