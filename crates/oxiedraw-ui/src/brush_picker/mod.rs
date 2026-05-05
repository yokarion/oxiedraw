//! Top-bar brush picker.
//!
//! Renders as a small icon button showing the active brush's custom
//! icon (or a generic fallback when the brush is icon-less). Clicking
//! opens a `gtk::Popover` containing a search entry, a three-dots
//! `MenuButton` (with stub "Manage Brushes" action - wired in stage
//! 7b), and a scrollable list of brushes. Each row shows the brush's
//! icon, name, and a Cairo-drawn sample stroke.
//!
//! Wired into `tool_options_bar` as a replacement for the old
//! `gtk::DropDown`. Listens on `BrushEngine::connect_brushes_changed`
//! so autoreload events refresh the list and trigger button live.

mod preview;
pub(crate) mod shared;

use std::cell::RefCell;
use std::rc::Rc;

use oxiedraw_core::brush_engine::{BrushEngine, BrushPresetId};
use relm4::gtk;
use relm4::gtk::prelude::*;

use shared::apply_icon_to_image;

use crate::settings::AppSettings;

const POPOVER_MAX_HEIGHT: i32 = 800;
const POPOVER_WIDTH: i32 = 360;
/// Outer button extent. GTK's default theme enforces a per-state
/// `min-height` that overrides `height_request`; the matching value
/// has to land on the inner `button` node via CSS (see `load_css_once`).
const TRIGGER_BUTTON_HEIGHT: i32 = 28;
const TRIGGER_BUTTON_WIDTH: i32 = 28;
const TRIGGER_ICON_SIZE: i32 = 28;
pub(super) const FALLBACK_ICON: &str = "oxiedraw-brush-symbolic";

/// Build the brush picker as a trigger button + a popover. Returns the
/// button widget; the popover is attached to it and lives as long as
/// the button.
pub(crate) fn build(
    brush_engine: &BrushEngine,
    default_brush_name: Rc<RefCell<Option<String>>>,
    toaster: crate::toaster::Toaster,
) -> gtk::Widget {
    let trigger = gtk::Button::builder()
        .width_request(TRIGGER_BUTTON_WIDTH)
        .height_request(TRIGGER_BUTTON_HEIGHT)
        .has_frame(false)
        .valign(gtk::Align::Center)
        .tooltip_text("Brushes")
        .build();
    trigger.add_css_class("brush-picker-trigger");
    trigger.add_css_class("flat");
    load_css_once();

    let trigger_image = gtk::Image::builder()
        .pixel_size(TRIGGER_ICON_SIZE)
        .icon_name(FALLBACK_ICON)
        .build();
    trigger.set_child(Some(&trigger_image));

    let popover = gtk::Popover::builder()
        .has_arrow(true)
        .autohide(true)
        .width_request(POPOVER_WIDTH)
        .build();
    popover.set_parent(&trigger);
    popover.add_css_class("brush-picker-popover");

    let trigger_image_for_active = trigger_image.clone();
    let brush_engine_for_active = brush_engine.clone();
    let refresh_trigger: Rc<dyn Fn()> = Rc::new(move || {
        update_trigger_image(&trigger_image_for_active, &brush_engine_for_active);
    });
    refresh_trigger();

    let content = build_popover_content(
        brush_engine.clone(),
        popover.clone(),
        refresh_trigger.clone(),
        default_brush_name,
        toaster,
    );
    popover.set_child(Some(&content));

    // Refresh trigger icon whenever the brush list changes (autoreload
    // or user repopulate).
    let refresh_trigger_for_engine = refresh_trigger.clone();
    brush_engine.connect_brushes_changed(Rc::new(move || {
        refresh_trigger_for_engine();
    }));

    trigger.connect_clicked(move |_| {
        popover.popup();
    });

    trigger.upcast()
}

fn build_popover_content(
    brush_engine: BrushEngine,
    popover: gtk::Popover,
    refresh_trigger: Rc<dyn Fn()>,
    default_brush_name: Rc<RefCell<Option<String>>>,
    toaster: crate::toaster::Toaster,
) -> gtk::Box {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();

    // ----- Header: search + three-dots -----
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search...")
        .hexpand(true)
        .build();
    header.append(&search);

    let menu_button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .has_frame(false)
        .build();
    let menu = build_menu();
    menu_button.set_menu_model(Some(&menu));
    header.append(&menu_button);
    outer.append(&header);

    // ----- Scrollable brush list -----
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .max_content_height(POPOVER_MAX_HEIGHT - 80)
        .vexpand(true)
        .build();
    let listbox = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .build();
    listbox.add_css_class("navigation-sidebar");
    scrolled.set_child(Some(&listbox));
    outer.append(&scrolled);

    // Row storage: parallel Vec<(BrushPresetId, gtk::ListBoxRow)> so we
    // can map row activation -> preset id and update selection on
    // `active` changes.
    let row_map: Rc<RefCell<Vec<(BrushPresetId, gtk::ListBoxRow)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let star_map: Rc<RefCell<Vec<(BrushPresetId, gtk::Button)>>> =
        Rc::new(RefCell::new(Vec::new()));

    let rebuild = {
        let brush_engine = brush_engine.clone();
        let listbox = listbox.clone();
        let row_map = row_map.clone();
        let star_map = star_map.clone();
        let search = search.clone();
        let default_brush_name = default_brush_name.clone();
        let toaster = toaster.clone();
        Rc::new(move || {
            rebuild_rows(
                &listbox,
                &brush_engine,
                &row_map,
                &search.text(),
                &star_map,
                &default_brush_name,
                &toaster,
            );
        })
    };
    rebuild();

    // Re-filter on search change.
    {
        let rebuild = rebuild.clone();
        search.connect_search_changed(move |_| {
            rebuild();
        });
    }

    // Rebuild + reselect when the engine's brush list changes
    // (autoreload / repopulate).
    {
        let rebuild = rebuild.clone();
        brush_engine.connect_brushes_changed(Rc::new(move || {
            rebuild();
        }));
    }

    // Row activation -> set active brush, refresh the trigger icon,
    // close the popover.
    {
        let brush_engine = brush_engine.clone();
        let row_map = row_map.clone();
        let popover = popover.clone();
        let refresh_trigger = refresh_trigger.clone();
        listbox.connect_row_activated(move |_, row| {
            if let Some((id, _)) = row_map.borrow().iter().find(|(_, r)| r == row) {
                brush_engine.active.set(*id);
                refresh_trigger();
                popover.popdown();
            }
        });
    }

    outer
}

fn build_menu() -> gtk::gio::Menu {
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Manage Brushes"), Some("app.brush-manager"));
    menu
}

fn rebuild_rows(
    listbox: &gtk::ListBox,
    brush_engine: &BrushEngine,
    row_map: &Rc<RefCell<Vec<(BrushPresetId, gtk::ListBoxRow)>>>,
    filter: &str,
    star_map: &Rc<RefCell<Vec<(BrushPresetId, gtk::Button)>>>,
    default_brush_name: &Rc<RefCell<Option<String>>>,
    toaster: &crate::toaster::Toaster,
) {
    while let Some(child) = listbox.first_child() {
        listbox.remove(&child);
    }
    row_map.borrow_mut().clear();
    star_map.borrow_mut().clear();

    let filter_lc = filter.trim().to_lowercase();
    let active_id = brush_engine.active.get();
    let def_name = default_brush_name.borrow().clone();
    let brushes = brush_engine.brushes.borrow();
    let mut selected_row: Option<gtk::ListBoxRow> = None;

    for preset in brushes.iter() {
        if !filter_lc.is_empty() && !preset.name.to_lowercase().contains(&filter_lc) {
            continue;
        }
        let is_default = def_name.as_deref() == Some(preset.name.as_str());
        let preset_name = preset.name.clone();
        let brush_id = preset.id;
        let default_brush_name_c = default_brush_name.clone();
        let toaster_c = toaster.clone();
        let star_map_c = star_map.clone();
        let on_set_default: Rc<dyn Fn()> = Rc::new(move || {
            *default_brush_name_c.borrow_mut() = Some(preset_name.clone());
            let mut settings = AppSettings::load();
            settings.default_brush_name = Some(preset_name.clone());
            settings.save();
            toaster_c.info(&format!("Default brush changed to: {preset_name}"));
            for (id, btn) in star_map_c.borrow().iter() {
                shared::update_star_icon(btn, *id == brush_id);
            }
        });
        let (row, star_btn) = shared::build_list_row(preset, is_default, Some(on_set_default));
        if let Some(btn) = star_btn {
            star_map.borrow_mut().push((preset.id, btn));
        }
        listbox.append(&row);
        if preset.id == active_id {
            selected_row = Some(row.clone());
        }
        row_map.borrow_mut().push((preset.id, row));
    }
    if let Some(row) = selected_row {
        listbox.select_row(Some(&row));
    }
}

/// Install once-per-process CSS for the picker trigger button. GTK's
/// css provider de-duplicates additions by priority + display, so even
/// if this is called multiple times the styles are only applied once.
fn load_css_once() {
    use std::sync::OnceLock;
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| {
        let provider = gtk::CssProvider::new();
        // GTK's default `button` node enforces its own `min-height`
        // (~24-30px depending on theme), so a bare `min-height` on
        // `.brush-picker-trigger` is overridden by the theme. We have
        // to push the override onto the button node *and* zero the
        // built-in padding so a 16px icon fits in a 30px box.
        provider.load_from_string(
            ".brush-picker-trigger,
             .brush-picker-trigger > * {
                min-height: 30px;
                min-width: 30px;
                padding: 0;
                margin: 0;
            }
            .brush-picker-trigger {
                border-radius: 4px;
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

fn update_trigger_image(image: &gtk::Image, brush_engine: &BrushEngine) {
    let id = brush_engine.active.get();
    let brushes = brush_engine.brushes.borrow();
    if let Some(preset) = brushes.iter().find(|p| p.id == id) {
        apply_icon_to_image(image, preset, FALLBACK_ICON);
    } else {
        image.set_icon_name(Some(FALLBACK_ICON));
    }
}
