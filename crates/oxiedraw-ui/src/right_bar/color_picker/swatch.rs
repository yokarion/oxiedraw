//! Primary / secondary slot swatch overlay.

use std::rc::Rc;

use oxiedraw_core::color::{Color, ColorSlot};
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::prelude::*;

use super::{PickerState, SWATCH_INNER, SWATCH_OFFSET, SWATCH_TOTAL};

pub(super) fn build_swatch_widget() -> gtk::DrawingArea {
    gtk::DrawingArea::builder()
        .content_width(SWATCH_TOTAL)
        .content_height(SWATCH_TOTAL)
        .margin_top(4)
        .margin_start(4)
        .build()
}

pub(super) fn install_swatch_draw(area: &gtk::DrawingArea, state: &PickerState) {
    let state = state.clone();
    area.set_draw_func(move |_, ctx, _w, _h| {
        let primary = state.colors.primary.get();
        let secondary = state.colors.secondary.get();
        let (back, front) = match state.colors.selected.get() {
            ColorSlot::Primary => (secondary, primary),
            ColorSlot::Secondary => (primary, secondary),
        };
        draw_swatch_box(ctx, SWATCH_OFFSET, 0.0, SWATCH_INNER, back);
        draw_swatch_box(ctx, 0.0, SWATCH_OFFSET, SWATCH_INNER, front);
    });
}

pub(super) fn install_swatch_input(
    area: &gtk::DrawingArea,
    state: &PickerState,
    refresh: &Rc<dyn Fn()>,
) {
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    let state = state.clone();
    let refresh = Rc::clone(refresh);
    click.connect_pressed(move |_, _, x, y| {
        // Back square is the one offset to the upper-right; the visible part
        // is anywhere it isn't covered by the front square.
        let on_back = x >= SWATCH_OFFSET
            && y <= SWATCH_OFFSET + SWATCH_INNER
            && (x > SWATCH_INNER || y < SWATCH_OFFSET);
        if on_back {
            let new_slot = match state.colors.selected.get() {
                ColorSlot::Primary => ColorSlot::Secondary,
                ColorSlot::Secondary => ColorSlot::Primary,
            };
            state.colors.selected.set(new_slot);
            state.load_hsv_from_current();
            refresh();
        }
    });
    area.add_controller(click);
}

fn draw_swatch_box(ctx: &cairo::Context, x: f64, y: f64, size: f64, color: Color) {
    ctx.rectangle(x, y, size, size);
    ctx.set_source_rgb(
        f64::from(color.r) / 255.0,
        f64::from(color.g) / 255.0,
        f64::from(color.b) / 255.0,
    );
    ctx.fill_preserve().ok();
    ctx.set_source_rgb(0.0, 0.0, 0.0);
    ctx.set_line_width(1.0);
    ctx.stroke().ok();
}
