//! Appearance page (window decoration toggle).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::settings::AppSettings;

pub(super) fn build_appearance_page(
    settings: Rc<RefCell<AppSettings>>,
    apply_decorations: Rc<dyn Fn(bool)>,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title("Appearance");
    page.set_icon_name(Some("applications-graphics-symbolic"));

    let window_group = adw::PreferencesGroup::new();
    window_group.set_title("Window");

    let row = adw::SwitchRow::new();
    row.set_title("Show window button decorations");
    row.set_subtitle("Display minimise, maximise and close buttons in the header bar");
    row.set_active(settings.borrow().appearance.show_window_decorations);

    row.connect_active_notify(move |r| {
        let visible = r.is_active();
        settings.borrow_mut().appearance.show_window_decorations = visible;
        settings.borrow().save();
        apply_decorations(visible);
    });

    window_group.add(&row);
    page.add(&window_group);
    page
}

// Keybinds page
