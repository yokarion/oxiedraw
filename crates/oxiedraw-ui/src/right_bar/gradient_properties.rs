//! Gradient tool panel: the stop bar plus Position/Opacity fields and a delete
//! button, shown above the colour picker while the tool is active. Selecting a
//! stop points the colour picker at it and editing the picker writes back; the
//! `syncing` and `active` flags keep that two-way link from looping.

use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::color::ColorState;
use oxiedraw_core::tools::GradientState;
use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::widgets::gradient_bar::{self, GradientBar};
use crate::widgets::boxed_list;

const MARGIN: i32 = 12;

/// Build the gradient panel. Returns the widget (a revealer) and a setter the
/// right-bar tool switch calls: `true` when the Gradient tool is active (show
/// + bind the picker), `false` otherwise.
pub(super) fn build(gradient: &GradientState, colors: &ColorState) -> (gtk::Widget, Rc<dyn Fn(bool)>) {
    let syncing = Rc::new(Cell::new(false));
    let active = Rc::new(Cell::new(false));

    // Transparent column (no card): the ramp sits directly on the sidebar,
    // the numeric controls go in a boxed list below it.
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(MARGIN)
        .margin_bottom(MARGIN)
        .margin_start(MARGIN)
        .margin_end(MARGIN)
        .build();

    // Position control: spin (0-100) with a trailing delete-stop button.
    let pos_adj = gtk::Adjustment::new(0.0, 0.0, 100.0, 1.0, 10.0, 0.0);
    let pos_spin = gtk::SpinButton::builder()
        .adjustment(&pos_adj)
        .climb_rate(1.0)
        .digits(0)
        .numeric(true)
        .halign(gtk::Align::End)
        .build();
    let delete_btn = gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .tooltip_text("Delete stop")
        .valign(gtk::Align::Center)
        .build();
    delete_btn.add_css_class("flat");

    // Opacity control: slider (0..1).
    let opac_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
    opac_scale.set_hexpand(true);
    opac_scale.set_draw_value(false);

    let delete_widget: gtk::Widget = delete_btn.clone().upcast();
    let list = boxed_list::list();
    list.append(&boxed_list::row("Position", &pos_spin, &[&delete_widget]));
    list.append(&boxed_list::row("Opacity", &opac_scale, &[]));

    // The bar is built after the field closures so its callbacks can drive
    // them; wire it in via a slot.
    let bar_slot: Rc<std::cell::RefCell<Option<GradientBar>>> =
        Rc::new(std::cell::RefCell::new(None));

    // Push the selected stop's values into the fields + the colour picker.
    let select_stop: Rc<dyn Fn(usize)> = {
        let gradient = gradient.clone();
        let colors = colors.clone();
        let syncing = Rc::clone(&syncing);
        let pos_spin = pos_spin.clone();
        let opac_scale = opac_scale.clone();
        Rc::new(move |idx: usize| {
            let settings = gradient.resolve(&colors);
            let Some(stop) = settings.stops.get(idx) else {
                return;
            };
            syncing.set(true);
            pos_spin.set_value(f64::from(stop.position) * 100.0);
            opac_scale.set_value(f64::from(stop.opacity));
            // Bind the picker to this stop's colour (wheel jumps to it).
            colors.set_current(stop.color);
            colors.notify_changed();
            syncing.set(false);
        })
    };

    let bar = gradient_bar::build(
        gradient,
        colors,
        Rc::clone(&select_stop),
        {
            // Stops changed (insert / move / delete): let external listeners
            // (e.g. the cursor overlay) know the ramp changed.
            let gradient = gradient.clone();
            Rc::new(move || gradient.notify_changed())
        },
    );
    bar.widget.set_margin_bottom(4);
    content.append(&bar.widget);
    content.append(&list);
    *bar_slot.borrow_mut() = Some(bar);

    // Position spin -> move the selected stop.
    {
        let gradient = gradient.clone();
        let colors = colors.clone();
        let syncing = Rc::clone(&syncing);
        let bar_slot = Rc::clone(&bar_slot);
        pos_spin.connect_value_changed(move |s| {
            if syncing.get() {
                return;
            }
            gradient.ensure_owned(&colors);
            let idx = gradient.selected_stop.get();
            #[allow(clippy::cast_possible_truncation)]
            let t = (s.value() / 100.0) as f32;
            let new_idx = gradient
                .settings
                .borrow_mut()
                .as_mut()
                .map_or(idx, |g| g.move_stop(idx, t));
            gradient.selected_stop.set(new_idx);
            gradient.notify_changed();
            if let Some(b) = bar_slot.borrow().as_ref() {
                b.refresh();
            }
        });
    }

    // Opacity slider -> set the selected stop's opacity.
    {
        let gradient = gradient.clone();
        let colors = colors.clone();
        let syncing = Rc::clone(&syncing);
        let bar_slot = Rc::clone(&bar_slot);
        opac_scale.connect_value_changed(move |s| {
            if syncing.get() {
                return;
            }
            gradient.ensure_owned(&colors);
            let idx = gradient.selected_stop.get();
            #[allow(clippy::cast_possible_truncation)]
            let opacity = s.value() as f32;
            if let Some(g) = gradient.settings.borrow_mut().as_mut()
                && let Some(stop) = g.stops.get_mut(idx)
            {
                stop.opacity = opacity;
            }
            gradient.notify_changed();
            if let Some(b) = bar_slot.borrow().as_ref() {
                b.refresh();
            }
        });
    }

    // Delete button -> remove the selected stop.
    {
        let gradient = gradient.clone();
        let colors = colors.clone();
        let bar_slot = Rc::clone(&bar_slot);
        let select_stop = Rc::clone(&select_stop);
        delete_btn.connect_clicked(move |_| {
            gradient.ensure_owned(&colors);
            let idx = gradient.selected_stop.get();
            let removed = gradient
                .settings
                .borrow_mut()
                .as_mut()
                .is_some_and(|g| g.remove_stop(idx));
            if removed {
                let len = gradient.settings.borrow().as_ref().map_or(1, |g| g.stops.len());
                let sel = idx.min(len - 1);
                gradient.selected_stop.set(sel);
                gradient.notify_changed();
                if let Some(b) = bar_slot.borrow().as_ref() {
                    b.refresh();
                }
                select_stop(sel);
            }
        });
    }

    // Picker -> selected stop colour (only while active + not self-syncing).
    {
        let gradient = gradient.clone();
        let colors_bus = colors.clone();
        let colors = colors.clone();
        let syncing = Rc::clone(&syncing);
        let active = Rc::clone(&active);
        let bar_slot = Rc::clone(&bar_slot);
        colors_bus.connect_changed(Box::new(move || {
            if !active.get() || syncing.get() {
                return;
            }
            gradient.ensure_owned(&colors);
            let idx = gradient.selected_stop.get();
            let c = colors.current();
            if let Some(g) = gradient.settings.borrow_mut().as_mut()
                && let Some(stop) = g.stops.get_mut(idx)
            {
                stop.color = c;
            }
            gradient.notify_changed();
            if let Some(b) = bar_slot.borrow().as_ref() {
                b.refresh();
            }
        }));
    }

    let revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .child(&content)
        .reveal_child(false)
        .build();

    let set_active: Rc<dyn Fn(bool)> = {
        let revealer = revealer.clone();
        let active = Rc::clone(&active);
        let select_stop = Rc::clone(&select_stop);
        let gradient = gradient.clone();
        Rc::new(move |on: bool| {
            active.set(on);
            revealer.set_reveal_child(on);
            if on {
                // Sync fields + picker to the current selection on entry.
                select_stop(gradient.selected_stop.get());
            }
        })
    };

    (revealer.upcast(), set_active)
}
