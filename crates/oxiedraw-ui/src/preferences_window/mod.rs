//! Preferences window (`app.preferences`): an `adw::PreferencesWindow` with
//! General, Canvas, Appearance and Keybinds pages. Changes are written to
//! `settings.json` immediately; there is no Apply/Cancel cycle.

mod appearance;
mod canvas;
mod general;
mod keybinds;
mod project;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{self, gdk, gio, glib};

use crate::actions::apply_all_accels;
use crate::session::AutosaveConfig;
use crate::settings::{AppSettings, PixelViewSettings};

use self::appearance::build_appearance_page;
use self::canvas::build_canvas_page;
use self::general::build_general_page;
use self::keybinds::{
    RowHandles, build_accel_string, build_keybinds_page, count_modified, is_modifier_key,
    refresh_row,
};
use self::project::build_project_page;

// Entry point

pub(crate) fn show(
    parent: &adw::ApplicationWindow,
    apply_decorations: Rc<dyn Fn(bool)>,
    apply_pixel_view: Rc<dyn Fn(&PixelViewSettings)>,
    autosave: AutosaveConfig,
) {
    let settings: Rc<RefCell<AppSettings>> = Rc::new(RefCell::new(AppSettings::load()));
    let recording_id: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let row_handles: Rc<RefCell<HashMap<String, RowHandles>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let modified_count: Rc<Cell<u32>> = Rc::new(Cell::new(0));

    let win = adw::PreferencesWindow::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(860)
        .default_height(620)
        .title("Preferences")
        .search_enabled(false)
        .build();

    win.add(&build_general_page());
    win.add(&build_canvas_page(
        Rc::clone(&settings),
        Rc::clone(&apply_pixel_view),
    ));
    win.add(&build_appearance_page(
        Rc::clone(&settings),
        apply_decorations,
    ));
    win.add(&build_project_page(Rc::clone(&settings), autosave));
    win.add(&build_keybinds_page(
        Rc::clone(&settings),
        Rc::clone(&recording_id),
        Rc::clone(&row_handles),
        Rc::clone(&modified_count),
    ));

    // Window-level key controller for keybind recording
    {
        let settings = Rc::clone(&settings);
        let recording_id = Rc::clone(&recording_id);
        let row_handles = Rc::clone(&row_handles);
        let modified_count = Rc::clone(&modified_count);

        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.connect_key_pressed(move |_, keyval, _, state| {
            let rec = recording_id.borrow().clone();
            let Some(action_id) = rec else {
                return glib::Propagation::Proceed;
            };

            if is_modifier_key(keyval) {
                return glib::Propagation::Stop;
            }

            let new_binding: Option<String> = if keyval == gdk::Key::Escape {
                *recording_id.borrow_mut() = None;
                refresh_row(
                    &action_id,
                    &row_handles.borrow(),
                    &settings.borrow(),
                    &recording_id,
                    None,
                );
                return glib::Propagation::Stop;
            } else if keyval == gdk::Key::BackSpace {
                None
            } else {
                Some(build_accel_string(keyval, state))
            };

            *recording_id.borrow_mut() = None;
            settings
                .borrow_mut()
                .keybinds
                .insert(action_id.clone(), new_binding);
            settings.borrow().save();

            if let Some(gio_app) = gio::Application::default()
                && let Ok(app) = gio_app.downcast::<gtk::Application>() {
                    apply_all_accels(&app, &settings.borrow());
                }

            let new_count = count_modified(&settings.borrow());
            modified_count.set(new_count);

            refresh_row(
                &action_id,
                &row_handles.borrow(),
                &settings.borrow(),
                &recording_id,
                Some(new_count),
            );

            glib::Propagation::Stop
        });
        win.add_controller(key_ctrl);
    }

    win.present();
}

// General page

pub(crate) fn load_keybind_css() {
    let css = r"
.keybind-recording-row {
    background: alpha(@accent_bg_color, 0.1);
}

.key-badge {
    border: 1px solid alpha(@borders, 0.8);
    border-radius: 4px;
    padding: 1px 6px;
    font-size: 12px;
    min-width: 20px;
    background: @card_bg_color;
}

.key-sep {
    font-size: 12px;
    color: alpha(@foreground_color, 0.5);
    padding: 0 1px;
}

.key-recording-placeholder {
    border: 2px dashed alpha(@accent_color, 0.6);
    border-radius: 4px;
}
";
    let provider = gtk::CssProvider::new();
    provider.load_from_string(css);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
