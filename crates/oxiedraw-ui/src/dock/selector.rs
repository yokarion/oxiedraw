use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::enum_meta::EnumMeta;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::layout::PanelId;

use super::manager::LayoutManager;

pub(crate) fn build(
    manager: &Rc<LayoutManager>,
    window: &adw::ApplicationWindow,
) -> gtk::Widget {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .valign(gtk::Align::Center)
        .margin_end(6)
        .build();
    bar.add_css_class("linked");
    crate::top_bar::style_control(&bar);

    // Set while the dropdown is repopulated, so the row that selects itself is
    // not mistaken for the user picking one.
    let syncing = Rc::new(Cell::new(false));

    let edit_toggle = gtk::ToggleButton::new();
    show_editing(&edit_toggle, false);

    let dropdown = gtk::DropDown::builder()
        .tooltip_text("Window layout")
        .width_request(150)
        .build();

    let name_entry = gtk::Entry::builder()
        .width_request(150)
        .placeholder_text("Layout name")
        .visible(false)
        .build();

    let duplicate = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Duplicate this layout")
        .build();
    let delete = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Delete this layout")
        .build();
    let panels = gtk::MenuButton::builder()
        .icon_name("oxiedraw-panels-symbolic")
        .tooltip_text("Panels shown in this layout")
        .build();
    let reset = gtk::Button::builder()
        .icon_name("edit-undo-symbolic")
        .tooltip_text("Reset this layout to the way it ships")
        .build();

    let checks = build_panel_list(&panels, manager);

    for widget in [
        edit_toggle.clone().upcast::<gtk::Widget>(),
        dropdown.clone().upcast(),
        name_entry.clone().upcast(),
        duplicate.clone().upcast(),
        delete.clone().upcast(),
        panels.clone().upcast(),
        reset.clone().upcast(),
    ] {
        bar.append(&widget);
    }

    let set_naming: Rc<dyn Fn(bool)> = {
        let dropdown = dropdown.clone();
        let name_entry = name_entry.clone();
        Rc::new(move |naming| {
            dropdown.set_visible(!naming);
            name_entry.set_visible(naming);
        })
    };

    let refresh: Rc<dyn Fn()> = {
        let manager = Rc::clone(manager);
        let dropdown = dropdown.clone();
        let syncing = Rc::clone(&syncing);
        let delete = delete.clone();
        let reset = reset.clone();
        let checks = checks.clone();
        Rc::new(move || {
            syncing.set(true);
            let names = manager.names();
            let rows: Vec<&str> = names.iter().map(String::as_str).collect();
            dropdown.set_model(Some(&gtk::StringList::new(&rows)));
            dropdown.set_selected(u32::try_from(manager.current_index()).unwrap_or(0));
            syncing.set(false);

            delete.set_visible(manager.edit_mode() && manager.can_delete());
            reset.set_sensitive(manager.current_is_preset());
            for (id, check) in checks.iter() {
                check.set_active(manager.is_panel_visible(*id));
            }
        })
    };
    refresh();
    manager.connect_changed(Rc::clone(&refresh));

    {
        let manager = Rc::clone(manager);
        edit_toggle.connect_toggled(move |b| manager.set_edit_mode(b.is_active()));
    }
    {
        let edit_only = [
            duplicate.clone().upcast::<gtk::Widget>(),
            panels.clone().upcast(),
            reset.clone().upcast(),
        ];
        let deletable = Rc::clone(manager);
        let delete = delete.clone();
        let edit_toggle = edit_toggle.clone();
        let set_naming = Rc::clone(&set_naming);
        let apply: Rc<dyn Fn(bool)> = Rc::new(move |on| {
            for widget in &edit_only {
                widget.set_visible(on);
            }
            delete.set_visible(on && deletable.can_delete());
            if edit_toggle.is_active() != on {
                edit_toggle.set_active(on);
            }
            show_editing(&edit_toggle, on);
            if !on {
                set_naming(false);
            }
        });
        apply(manager.edit_mode());
        manager.connect_edit_mode(apply);
    }

    {
        let manager = Rc::clone(manager);
        let syncing = Rc::clone(&syncing);
        dropdown.connect_selected_notify(move |dd| {
            if syncing.get() {
                return;
            }
            let index = dd.selected() as usize;
            if let Some(name) = manager.names().get(index) {
                manager.select(name);
            }
        });
    }

    {
        let manager = Rc::clone(manager);
        let name_entry = name_entry.clone();
        let set_naming = Rc::clone(&set_naming);
        duplicate.connect_clicked(move |_| {
            let name = manager.duplicate();
            name_entry.set_text(&name);
            set_naming(true);
            name_entry.grab_focus();
            name_entry.select_region(0, -1);
        });
    }
    {
        let manager = Rc::clone(manager);
        let set_naming = Rc::clone(&set_naming);
        name_entry.connect_activate(move |entry| {
            manager.rename_current(entry.text().as_str());
            set_naming(false);
        });
    }
    {
        let set_naming = Rc::clone(&set_naming);
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                set_naming(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        name_entry.add_controller(keys);
    }

    {
        let manager = Rc::clone(manager);
        let window = window.clone();
        delete.connect_clicked(move |_| confirm_delete(&manager, &window));
    }

    {
        let manager = Rc::clone(manager);
        reset.connect_clicked(move |_| manager.reset());
    }

    bar.upcast()
}

const EDITING_CLASS: &str = "oxiedraw-layout-editing";

fn show_editing(toggle: &gtk::ToggleButton, editing: bool) {
    load_css();
    if editing {
        toggle.set_icon_name("object-select-symbolic");
        toggle.set_tooltip_text(Some("Done - finish editing this layout"));
        toggle.add_css_class(EDITING_CLASS);
    } else {
        toggle.set_icon_name("document-edit-symbolic");
        toggle.set_tooltip_text(Some(
            "Edit this layout - move, resize or switch off panels",
        ));
        toggle.remove_css_class(EDITING_CLASS);
    }
}

fn load_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            ".oxiedraw-layout-editing {
                color: @success_color;
            }",
        );
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

fn build_panel_list(
    button: &gtk::MenuButton,
    manager: &Rc<LayoutManager>,
) -> Rc<Vec<(PanelId, gtk::CheckButton)>> {
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();

    let mut checks = Vec::new();
    for id in PanelId::ALL {
        let spec = id.spec();
        let check = gtk::CheckButton::with_label(spec.display_name);
        let sides = match (spec.allows_horizontal(), spec.allows_vertical()) {
            (true, true) => "Docks against any edge.",
            (true, false) => "Docks along the top or bottom only.",
            (false, true) => "Docks down the left or right only.",
            (false, false) => "Fixed in place.",
        };
        check.set_tooltip_text(Some(&format!("{}\n{sides}", spec.description)));
        check.set_active(manager.is_panel_visible(*id));
        check.set_sensitive(spec.removable);
        {
            let manager = Rc::clone(manager);
            let id = *id;
            check.connect_toggled(move |c| {
                if manager.is_panel_visible(id) != c.is_active() {
                    manager.set_panel_visible(id, c.is_active());
                }
            });
        }
        list.append(&check);
        checks.push((*id, check));
    }

    button.set_popover(Some(&gtk::Popover::builder().child(&list).build()));
    Rc::new(checks)
}

fn confirm_delete(manager: &Rc<LayoutManager>, window: &adw::ApplicationWindow) {
    let name = manager.current_name();
    let dialog = gtk::AlertDialog::builder()
        .message(format!("Delete \"{name}\"?"))
        .detail("This layout will be removed. Layouts cannot be brought back.")
        .modal(true)
        .build();
    dialog.set_buttons(&["Cancel", "Delete"]);
    dialog.set_cancel_button(0);
    dialog.set_default_button(0);

    let manager = Rc::clone(manager);
    dialog.choose(
        Some(window),
        None::<&gtk::gio::Cancellable>,
        move |result| {
            if result == Ok(1) {
                manager.remove(&name);
            }
        },
    );
}
