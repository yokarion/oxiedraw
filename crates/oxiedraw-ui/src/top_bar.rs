use adw::prelude::*;
use gtk::gio;
use relm4::RelmWidgetExt;

use crate::settings::AppSettings;

/// Build the top bar and return it alongside a callback that shows or hides
/// the window control buttons (minimise / maximise / close).
pub(crate) fn build() -> (gtk::WindowHandle, impl Fn(bool) + 'static) {
    load_css();
    let handle = gtk::WindowHandle::new();

    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
        .height_request(40)
        .build();
    bar.add_css_class("menubar");

    let left_controls = gtk::WindowControls::builder()
        .side(gtk::PackType::Start)
        .valign(gtk::Align::Center)
        .build();
    bar.append(&left_controls);

    let menus: &[(&str, gio::MenuModel)] = &[
        ("File", build_file_menu().upcast()),
        ("Edit", build_edit_menu().upcast()),
        ("Select", build_select_menu().upcast()),
        ("View", build_view_menu().upcast()),
        ("Image", build_image_menu().upcast()),
        ("Filters", build_filters_menu().upcast()),
    ];

    for (label, model) in menus {
        let btn = gtk::MenuButton::builder()
            .label(*label)
            .menu_model(model)
            .valign(gtk::Align::Center)
            .build();
        btn.add_css_class("flat");
        btn.add_css_class("menubar-item");
        btn.inline_css("font-size: 13px; padding-top: 0; padding-bottom: 0; padding-left: 6px; padding-right: 6px;");

        bar.append(&btn);
    }

    let spacer = gtk::Box::builder().hexpand(true).build();
    bar.append(&spacer);

    // Primary (gear) menu button
    let primary_btn = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .menu_model(&build_primary_menu().upcast::<gio::MenuModel>())
        .valign(gtk::Align::Center)
        .build();
    primary_btn.add_css_class("flat");
    primary_btn.inline_css("padding-top: 0; padding-bottom: 0;");
    bar.append(&primary_btn);

    let right_controls = gtk::WindowControls::builder()
        .side(gtk::PackType::End)
        .margin_end(8)
        .margin_start(4)
        .valign(gtk::Align::Center)
        .build();
    bar.append(&right_controls);

    handle.set_child(Some(&bar));

    // Apply initial visibility from saved settings
    let show = AppSettings::load().appearance.show_window_decorations;
    left_controls.set_visible(show);
    right_controls.set_visible(show);

    // Callback used by the preferences window to update controls in real time
    let lc = left_controls;
    let rc = right_controls;
    let apply = move |visible: bool| {
        lc.set_visible(visible);
        rc.set_visible(visible);
    };

    (handle, apply)
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".menubar-item > toggle {
            min-height: 20px;
            padding-top: 0;
            padding-bottom: 0;
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn item(label: &str, action: &str, accel: Option<&str>) -> gio::MenuItem {
    let it = gio::MenuItem::new(Some(label), Some(action));
    if let Some(a) = accel {
        it.set_attribute_value("accel", Some(&a.to_variant()));
    }
    it
}

fn build_file_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let s1 = gio::Menu::new();
    s1.append_item(&item("New", "app.new", None));
    s1.append_item(&item("Open...", "app.open", None));
    menu.append_section(None, &s1);

    let s2 = gio::Menu::new();
    let recent = gio::Menu::new();
    let recent_item = gio::MenuItem::new_submenu(Some("Open Recent"), &recent);
    s2.append_item(&recent_item);
    menu.append_section(None, &s2);

    let s3 = gio::Menu::new();
    s3.append_item(&item("Save", "app.save", None));
    s3.append_item(&item("Save As...", "app.save-as", None));
    s3.append_item(&item("Export As...", "app.export-as", None));
    menu.append_section(None, &s3);

    let s4 = gio::Menu::new();
    s4.append_item(&item("Close Tab", "app.close-tab", None));
    s4.append_item(&item("Quit", "app.quit", None));
    menu.append_section(None, &s4);

    menu
}

fn build_edit_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let s1 = gio::Menu::new();
    s1.append_item(&item("Undo", "app.undo", None));
    s1.append_item(&item("Redo", "app.redo", None));
    menu.append_section(None, &s1);

    let s2 = gio::Menu::new();
    s2.append_item(&item("Cut", "app.cut", None));
    s2.append_item(&item("Copy", "app.copy", None));
    s2.append_item(&item("Paste", "app.paste", None));
    menu.append_section(None, &s2);

    menu
}

fn build_select_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let s1 = gio::Menu::new();
    s1.append_item(&item("Deselect", "app.deselect-all", None));
    menu.append_section(None, &s1);
    menu
}

fn build_view_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let s1 = gio::Menu::new();
    s1.append_item(&item("Zoom In", "app.zoom-in", None));
    s1.append_item(&item("Zoom Out", "app.zoom-out", None));
    s1.append_item(&item("Zoom to Fit", "app.zoom-fit", None));
    menu.append_section(None, &s1);

    let s2 = gio::Menu::new();
    s2.append_item(&item("Full Screen", "app.fullscreen", None));
    s2.append_item(&item("Performance Graph", "app.perf-graph", None));
    menu.append_section(None, &s2);

    menu
}

fn build_image_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let s1 = gio::Menu::new();
    s1.append_item(&item("Canvas Size...", "app.canvas-size", None));
    s1.append_item(&item("Resize Canvas...", "app.resize-canvas", None));
    menu.append_section(None, &s1);

    let s2 = gio::Menu::new();
    s2.append_item(&item("Flatten Image", "app.flatten-image", None));
    menu.append_section(None, &s2);

    menu
}

fn build_filters_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let adjust = gio::Menu::new();
    adjust.append_item(&item("Hue/Saturation/Value...", "app.filter-hsv", None));
    adjust.append_item(&item("Invert", "app.filter-invert", None));
    menu.append_submenu(Some("Adjust"), &adjust);

    let blur_sharpen = gio::Menu::new();
    blur_sharpen.append_item(&item("Blur...", "app.filter-blur", None));
    blur_sharpen.append_item(&item("Sharpen...", "app.filter-sharpen", None));
    menu.append_submenu(Some("Blur/Sharpen"), &blur_sharpen);

    menu
}

fn build_primary_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let s1 = gio::Menu::new();
    s1.append_item(&item("Manage Brushes...", "app.brush-manager", None));
    menu.append_section(None, &s1);
    let s2 = gio::Menu::new();
    s2.append_item(&item("Preferences", "app.preferences", None));
    menu.append_section(None, &s2);
    menu
}
