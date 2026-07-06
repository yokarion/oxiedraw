//! General page (read-only info rows).

use adw::prelude::*;

use crate::settings::APP_VERSION;

pub(super) fn build_general_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title("General");
    page.set_icon_name(Some("weather-clear-symbolic"));

    // App identity - custom widget in a borderless group
    let identity_group = adw::PreferencesGroup::new();

    let identity = gtk::Box::new(gtk::Orientation::Vertical, 8);
    identity.set_halign(gtk::Align::Center);
    identity.set_margin_top(12);
    identity.set_margin_bottom(12);

    let icon = gtk::Image::from_icon_name(crate::APP_ID);
    icon.set_pixel_size(80);
    identity.append(&icon);

    let name_lbl = gtk::Label::new(Some("OxieDraw"));
    name_lbl.add_css_class("title-2");
    identity.append(&name_lbl);

    let build_date = option_env!("OXIEDRAW_BUILD_DATE").unwrap_or(" - ");
    let channel = option_env!("OXIEDRAW_CHANNEL").unwrap_or("dev");
    let version_str = format!("v{APP_VERSION}  .  {channel}  .  {build_date}");
    let version_lbl = gtk::Label::new(Some(&version_str));
    version_lbl.add_css_class("dim-label");
    identity.append(&version_lbl);

    identity_group.add(&identity);
    page.add(&identity_group);

    // About group
    let about_group = adw::PreferencesGroup::new();
    about_group.set_title("About");

    let version_row = adw::ActionRow::new();
    version_row.set_title("Version");
    version_row.set_subtitle("The version of OxieDraw you're running");
    let version_val = gtk::Label::new(Some(APP_VERSION));
    version_val.set_valign(gtk::Align::Center);
    version_val.add_css_class("dim-label");
    version_row.add_suffix(&version_val);
    about_group.add(&version_row);

    let build_row = adw::ActionRow::new();
    build_row.set_title("Build");
    build_row.set_subtitle("Updated on every commit to main");
    let build_val = gtk::Label::new(Some(&format!("{channel}  .  {build_date}")));
    build_val.set_valign(gtk::Align::Center);
    build_val.add_css_class("dim-label");
    build_row.add_suffix(&build_val);
    about_group.add(&build_row);

    let notes_row = adw::ActionRow::new();
    notes_row.set_title("Release notes");
    notes_row.set_subtitle("What changed in this version");
    let link_icon = gtk::Image::from_icon_name("external-link-symbolic");
    link_icon.set_valign(gtk::Align::Center);
    link_icon.add_css_class("dim-label");
    notes_row.add_suffix(&link_icon);
    about_group.add(&notes_row);

    page.add(&about_group);
    page
}

// Canvas page
