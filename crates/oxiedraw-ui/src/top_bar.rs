use adw::prelude::*;
use gtk::gio;
use relm4::RelmWidgetExt;

use crate::settings::AppSettings;

pub(crate) const CONTROL_HEIGHT: i32 = 30;

const CONTROL_CLASS: &str = "oxiedraw-topbar-control";

// The request only lands because `load_css` takes the padding and minimum out
// of the button inside; left alone each one overshoots by a different amount.
pub(crate) fn style_control(widget: &impl IsA<gtk::Widget>) {
    load_css();
    let widget = widget.as_ref();
    widget.add_css_class(CONTROL_CLASS);
    widget.set_height_request(CONTROL_HEIGHT);
}

pub(crate) fn build(layout_control: &gtk::Widget) -> (gtk::WindowHandle, impl Fn(bool) + 'static) {
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
        ("Filters", build_filters_menu().upcast()),
    ];

    for (label, model) in menus {
        // Nested: a sliding submenu taller than its menu gets cropped.
        let popover = gtk::PopoverMenu::from_model_full(model, gtk::PopoverMenuFlags::NESTED);
        let btn = gtk::MenuButton::builder()
            .label(*label)
            .popover(&popover)
            .valign(gtk::Align::Center)
            .build();
        btn.add_css_class("flat");
        btn.add_css_class("menubar-item");
        btn.inline_css("font-size: 13px; padding-top: 0; padding-bottom: 0; padding-left: 6px; padding-right: 6px;");

        bar.append(&btn);
    }

    let spacer = gtk::Box::builder().hexpand(true).build();
    bar.append(&spacer);

    bar.append(layout_control);
    bar.append(&build_guide_control());

    let primary_btn = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .menu_model(&build_primary_menu().upcast::<gio::MenuModel>())
        .valign(gtk::Align::Center)
        .build();
    primary_btn.add_css_class("flat");
    style_control(&primary_btn);
    bar.append(&primary_btn);

    let right_controls = gtk::WindowControls::builder()
        .side(gtk::PackType::End)
        .margin_end(8)
        .margin_start(4)
        .valign(gtk::Align::Center)
        .build();
    bar.append(&right_controls);

    handle.set_child(Some(&bar));

    let show = AppSettings::load().appearance.show_window_decorations;
    left_controls.set_visible(show);
    right_controls.set_visible(show);

    let lc = left_controls;
    let rc = right_controls;
    let apply = move |visible: bool| {
        lc.set_visible(visible);
        rc.set_visible(visible);
    };

    (handle, apply)
}

fn build_guide_control() -> gtk::ToggleButton {
    let toggle = gtk::ToggleButton::builder()
        .icon_name("oxiedraw-guide-symbolic")
        .tooltip_text("Symmetry - switch the drawing guide on or off")
        .action_name("app.guide-toggle")
        .valign(gtk::Align::Center)
        .margin_end(6)
        .build();
    style_control(&toggle);

    apply_guide_style(&toggle, toggle.is_active());
    toggle.connect_active_notify(|b| apply_guide_style(b, b.is_active()));
    toggle
}

fn apply_guide_style(toggle: &gtk::ToggleButton, on: bool) {
    if on {
        toggle.remove_css_class("flat");
        toggle.add_css_class("suggested-action");
    } else {
        toggle.remove_css_class("suggested-action");
        toggle.add_css_class("flat");
    }
}

fn load_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            ".menubar-item > toggle {
                min-height: 20px;
                padding-top: 0;
                padding-bottom: 0;
            }

            /* Down to nothing, so the height request on a control is the height
               it comes out at. Spelled out child by child rather than as a
               descendant selector, which would reach into the popovers too. */
            .oxiedraw-topbar-control,
            .oxiedraw-topbar-control > button,
            .oxiedraw-topbar-control > dropdown > button,
            .oxiedraw-topbar-control > menubutton > button,
            .oxiedraw-topbar-control > entry,
            .oxiedraw-topbar-control > entry > text {
                min-height: 0;
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
    });
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

fn build_filters_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let adjust = gio::Menu::new();
    adjust.append_item(&item("Hue/Saturation/Value...", "app.filter-hsv", None));
    adjust.append_item(&item("Curves...", "app.filter-curves", None));
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
