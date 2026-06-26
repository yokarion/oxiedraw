use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::filters::FilterSpec;
use relm4::gtk;
use relm4::gtk::prelude::*;

use super::{FilterContext, open_adjustable};
use crate::widgets::{boxed_list, slider};

pub(crate) fn show_blur(ctx: &FilterContext) {
    open_adjustable(
        ctx,
        "Blur",
        FilterSpec::BoxBlur {
            radius_x: 0.0,
            radius_y: 0.0,
        },
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

            let type_combo = gtk::DropDown::from_strings(&["Box Blur"]);
            list.append(&boxed_list::row("Type", &type_combo, &[]));

            // Lock links the two radii so they move together (default on).
            let locked = Rc::new(Cell::new(true));
            let h_scale = slider::build((0.0, 100.0), 1.0, 0.0, 200, fmt_px, {
                let spec = Rc::clone(spec);
                let push = push.clone();
                move |v| {
                    update_blur(&spec, |b| b.radius_x = v as f32);
                    push();
                }
            });
            let v_scale = slider::build((0.0, 100.0), 1.0, 0.0, 200, fmt_px, {
                let spec = Rc::clone(spec);
                let push = push.clone();
                move |v| {
                    update_blur(&spec, |b| b.radius_y = v as f32);
                    push();
                }
            });

            wire_radius_lock(&h_scale, &v_scale, &locked);

            let lock_btn = gtk::ToggleButton::builder()
                .icon_name("changes-prevent-symbolic")
                .active(true)
                .tooltip_text("Link horizontal and vertical blur")
                .build();
            lock_btn.add_css_class("flat");
            {
                let locked = Rc::clone(&locked);
                let v_scale = v_scale.clone();
                let h_scale = h_scale.clone();
                lock_btn.connect_toggled(move |b| {
                    locked.set(b.is_active());
                    b.set_icon_name(if b.is_active() {
                        "changes-prevent-symbolic"
                    } else {
                        "changes-allow-symbolic"
                    });
                    if b.is_active() {
                        v_scale.set_value(h_scale.value());
                    }
                });
            }

            list.append(&boxed_list::row(
                "Horizontal",
                &h_scale,
                &[lock_btn.upcast_ref()],
            ));
            list.append(&boxed_list::row("Vertical", &v_scale, &[]));
            content.append(&list);
        },
    );
}

/// Keep the two scales in sync while locked, guarding against the re-entrant
/// value-changed the programmatic set would trigger.
fn wire_radius_lock(h_scale: &gtk::Scale, v_scale: &gtk::Scale, locked: &Rc<Cell<bool>>) {
    let syncing = Rc::new(Cell::new(false));
    {
        let other = v_scale.clone();
        let locked = Rc::clone(locked);
        let syncing = Rc::clone(&syncing);
        h_scale.connect_value_changed(move |s| {
            if locked.get() && !syncing.get() {
                syncing.set(true);
                other.set_value(s.value());
                syncing.set(false);
            }
        });
    }
    {
        let other = h_scale.clone();
        let locked = Rc::clone(locked);
        let syncing = Rc::clone(&syncing);
        v_scale.connect_value_changed(move |s| {
            if locked.get() && !syncing.get() {
                syncing.set(true);
                other.set_value(s.value());
                syncing.set(false);
            }
        });
    }
}

fn update_blur(spec: &Rc<Cell<FilterSpec>>, f: impl FnOnce(&mut BlurFields)) {
    if let FilterSpec::BoxBlur { radius_x, radius_y } = spec.get() {
        let mut fields = BlurFields { radius_x, radius_y };
        f(&mut fields);
        spec.set(FilterSpec::BoxBlur {
            radius_x: fields.radius_x,
            radius_y: fields.radius_y,
        });
    }
}

#[allow(clippy::cast_possible_truncation)]
fn fmt_px(v: f64) -> String {
    format!("{} px", v.round() as i64)
}

struct BlurFields {
    radius_x: f32,
    radius_y: f32,
}
