use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use super::binding_rows::{BindingRows, LabelFn, pan_mode_switch};
use super::test_pad::{self, PadHooks};
use crate::input::{self, PenAction, PenInput, Trigger};
use crate::widgets::curve_editor::CurveEditor;

pub(super) fn build_page(tablet: &str) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let name: Rc<str> = Rc::from(tablet);

    let pressure = adw::PreferencesGroup::builder()
        .title("Pressure")
        .description("The bottom bar is how hard you press, the right bar the pressure the brush gets.")
        .build();
    let editor = CurveEditor::new();
    editor.set_meter_style();
    editor.set_curve(input::read(|s| s.pen(&name).pressure));
    {
        let name = Rc::clone(&name);
        editor.connect_changed(move |curve| input::update_unsaved(|s| s.pen_mut(&name).pressure = curve));
    }
    save_when_settled(&editor.widget);

    let actions = adw::PreferencesGroup::builder().title("Actions").build();
    let rows = BindingRows::<PenAction>::new(&actions, pen_label(Rc::clone(&name)));
    rows.sync(&input::read(|s| s.pen(&name).bindings), None);
    {
        let name = Rc::clone(&name);
        rows.connect(move |action, choice| {
            input::update(|s| {
                let pen = s.pen_mut(&name);
                let displaced = pen.assign(action, choice);
                (pen.bindings.clone(), displaced)
            })
        });
    }
    {
        let name = Rc::clone(&name);
        let current = input::read(|s| s.pen(&name).pan_mode);
        rows.add_selector(
            PenAction::Pan,
            &pan_mode_switch(current, move |mode| input::update(|s| s.pen_mut(&name).pan_mode = mode)),
        );
    }

    editor.widget.set_size_request(test_pad::SIDE, test_pad::SIDE);
    editor.widget.set_hexpand(false);
    let columns = gtk::Box::builder().spacing(12).homogeneous(true).halign(gtk::Align::Center).build();
    columns.append(&editor.widget);
    let hooks = PadHooks {
        on_pressure: Box::new(move |raw| editor.set_marker(raw)),
        on_news: {
            let rows = Rc::clone(&rows);
            let name = Rc::clone(&name);
            Box::new(move || {
                rows.relabel();
                rows.sync(&input::read(|s| s.pen(&name).bindings), None);
            })
        },
    };
    columns.append(&test_pad::build(&name, hooks));
    pressure.add(&columns);
    page.add(&pressure);
    page.add(&actions);
    page
}

fn save_when_settled(widget: &gtk::DrawingArea) {
    let settled = gtk::EventControllerLegacy::new();
    settled.set_propagation_phase(gtk::PropagationPhase::Capture);
    settled.connect_event(|_, event| {
        if matches!(
            event.event_type(),
            gdk::EventType::ButtonRelease | gdk::EventType::TouchEnd | gdk::EventType::KeyRelease
        ) {
            input::save_pending();
        }
        glib::Propagation::Proceed
    });
    widget.add_controller(settled);
    widget.connect_unmap(|_| input::save_pending());
}

fn pen_label(name: Rc<str>) -> LabelFn<PenInput> {
    Rc::new(move |choice| {
        let Some(pen_input) = choice else {
            return "Unbind".to_owned();
        };
        let detected = input::read(|s| {
            s.tablets.get(&*name).is_some_and(|p| match pen_input {
                PenInput::Tip => false,
                PenInput::EraserTip => p.eraser_detected,
                _ => p.detected_buttons.contains(&pen_input),
            })
        });
        if detected {
            format!("{} (detected)", pen_input.label())
        } else {
            pen_input.label().to_owned()
        }
    })
}
