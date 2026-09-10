use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::tools::{Tool, ToolState};
use relm4::RelmWidgetExt;
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::prelude::*;

use crate::settings::AppSettings;
use crate::settings::keybinds::accel_parts_for;

const SIZE: i32 = 40;
const INDICATOR_SIZE: f64 = 7.0;
const ACTIVE_CSS_CLASS: &str = "accent";

// `programmatic` is the guard that stops the setter recursing: with it set, the
// `toggled` handler restyles and nothing more.
pub(super) fn build(
    group_name: &'static str,
    action_id: Option<&'static str>,
    subtools: &'static [Tool],
    tools: &ToolState,
    on_change: &Rc<dyn Fn(Tool)>,
    programmatic: Rc<Cell<bool>>,
) -> (gtk::Overlay, gtk::ToggleButton, Rc<Cell<Tool>>) {
    let active_tool = tools.active.get();
    let is_active = subtools.contains(&active_tool);
    let initial = if is_active {
        active_tool
    } else {
        subtools.first().copied().unwrap_or(Tool::Brush)
    };

    let active_subtool = Rc::new(Cell::new(initial));

    let tooltip = build_tooltip(group_name, action_id);
    let btn = gtk::ToggleButton::builder()
        .icon_name(initial.icon_name())
        .tooltip_text(tooltip)
        .width_request(SIZE)
        .height_request(SIZE)
        .active(is_active)
        .build();
    btn.add_css_class("flat");
    btn.inline_css("border-radius: 0%");
    if is_active {
        btn.add_css_class(ACTIVE_CSS_CLASS);
    }

    {
        let on_change_c = Rc::clone(on_change);
        let active_sub_c = Rc::clone(&active_subtool);
        let prog_c = Rc::clone(&programmatic);
        btn.connect_toggled(move |b| {
            if b.is_active() {
                b.add_css_class(ACTIVE_CSS_CLASS);
            } else {
                b.remove_css_class(ACTIVE_CSS_CLASS);
            }
            if prog_c.get() {
                return;
            }
            if b.is_active() {
                on_change_c(active_sub_c.get());
            }
        });
    }

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&btn));

    if subtools.len() > 1 {
        let indicator = gtk::DrawingArea::builder().can_target(false).build();
        indicator.set_draw_func(|_, cr, w, h| {
            draw_indicator(cr, w, h);
        });
        overlay.add_overlay(&indicator);

        let popover = build_popover(subtools, &active_subtool, &btn, on_change);
        popover.set_parent(&btn);

        let popover_for_click = popover.clone();
        let right_click = gtk::GestureClick::new();
        right_click.set_button(3);
        right_click.connect_released(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            popover_for_click.popup();
        });
        btn.add_controller(right_click);

        let popover_for_dbl = popover.clone();
        let dbl_click = gtk::GestureClick::new();
        dbl_click.set_button(1);
        // Capture phase, or the ToggleButton's own gesture consumes the press
        // before the double-press is counted.
        dbl_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        dbl_click.connect_released(move |gesture, n_press, _, _| {
            if n_press >= 2 {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                popover_for_dbl.popup();
            }
        });
        btn.add_controller(dbl_click);

        btn.connect_destroy(move |_| {
            popover.unparent();
        });
    }

    (overlay, btn, active_subtool)
}

fn build_tooltip(name: &str, action_id: Option<&str>) -> String {
    let Some(id) = action_id else { return name.to_string() };
    let settings = AppSettings::load();
    match accel_parts_for(id, &settings) {
        Some(parts) if !parts.is_empty() => format!("{name} ({})", parts.join("+")),
        _ => name.to_string(),
    }
}

fn draw_indicator(cr: &cairo::Context, w: i32, h: i32) {
    let xf = f64::from(w);
    let yf = f64::from(h);
    cr.move_to(xf - INDICATOR_SIZE, yf);
    cr.line_to(xf, yf);
    cr.line_to(xf, yf - INDICATOR_SIZE);
    cr.close_path();
    cr.set_source_rgba(0.75, 0.75, 0.75, 0.9);
    let _ = cr.fill();
}

fn build_popover(
    subtools: &'static [Tool],
    active_subtool: &Rc<Cell<Tool>>,
    group_btn: &gtk::ToggleButton,
    on_change: &Rc<dyn Fn(Tool)>,
) -> gtk::Popover {
    let popover = gtk::Popover::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
    content.set_margin_top(4);
    content.set_margin_bottom(4);
    content.set_margin_start(2);
    content.set_margin_end(2);

    for &subtool in subtools {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_start(4)
            .margin_end(8)
            .build();
        row.append(&gtk::Image::from_icon_name(subtool.icon_name()));
        let lbl = gtk::Label::new(Some(subtool.display_name()));
        lbl.set_halign(gtk::Align::Start);
        lbl.set_hexpand(true);
        row.append(&lbl);

        let sub_btn = gtk::Button::new();
        sub_btn.set_child(Some(&row));
        sub_btn.add_css_class("flat");

        let active_sub_c = Rc::clone(active_subtool);
        let group_btn_c = group_btn.clone();
        let on_change_c = Rc::clone(on_change);
        let popover_c = popover.clone();

        sub_btn.connect_clicked(move |_| {
            active_sub_c.set(subtool);
            group_btn_c.set_icon_name(subtool.icon_name());
            group_btn_c.set_tooltip_text(Some(subtool.display_name()));
            if group_btn_c.is_active() {
                on_change_c(subtool);
            } else {
                group_btn_c.set_active(true);
            }
            popover_c.popdown();
        });

        content.append(&sub_btn);
    }

    popover.set_child(Some(&content));
    popover
}
