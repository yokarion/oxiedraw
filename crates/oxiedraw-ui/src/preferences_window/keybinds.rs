//! Keybind page: per-action rows, click to record a new shortcut.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{self, gdk, gio};

use crate::actions::apply_all_accels;
use crate::settings::AppSettings;
use crate::settings::keybinds::{ALL_ACTION_GROUPS, ActionInfo, format_accel};

pub(super) struct RowHandles {
    /// `None` for the sentinel `"__reset_all__"` entry.
    row: Option<adw::ActionRow>,
    key_suffix_box: gtk::Box,
    /// For real rows: the per-row <- reset button.
    /// For the `"__reset_all__"` sentinel: the global reset-all button.
    reset_all_btn: gtk::Button,
    /// Per-row reset button; same widget as `reset_all_btn` for real rows  - 
    /// stored separately so `refresh_row` can toggle its visibility.
    row_reset_btn: gtk::Button,
}

pub(super) fn build_keybinds_page(
    settings: Rc<RefCell<AppSettings>>,
    recording_id: Rc<RefCell<Option<String>>>,
    row_handles: Rc<RefCell<HashMap<String, RowHandles>>>,
    modified_count: Rc<Cell<u32>>,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title("Keybinds");
    page.set_icon_name(Some("input-keyboard-symbolic"));

    // -- Header group: info label + reset-all button ---------------------------
    let total_actions: usize = ALL_ACTION_GROUPS.iter().map(|g| g.actions.len()).sum();
    let n_groups = ALL_ACTION_GROUPS.len();

    let header_group = adw::PreferencesGroup::new();
    header_group.set_description(Some(&format!(
        "{total_actions} shortcuts across {n_groups} groups.\nClick a shortcut to rebind; press Backspace to unbind, Escape to cancel."
    )));

    let initial_count = count_modified(&settings.borrow());
    modified_count.set(initial_count);

    let reset_all_btn = gtk::Button::new();
    reset_all_btn.add_css_class("pill");
    update_reset_all_label(&reset_all_btn, initial_count);

    // Store reset_all_btn reference in row_handles under a sentinel key
    let dummy_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let dummy_btn = gtk::Button::new();
    row_handles.borrow_mut().insert(
        "__reset_all__".to_string(),
        RowHandles {
            row: None,
            key_suffix_box: dummy_box,
            reset_all_btn: reset_all_btn.clone(),
            row_reset_btn: dummy_btn,
        },
    );

    {
        let settings = Rc::clone(&settings);
        let recording_id = Rc::clone(&recording_id);
        let row_handles = Rc::clone(&row_handles);
        let modified_count = Rc::clone(&modified_count);
        let reset_all_btn2 = reset_all_btn.clone();
        reset_all_btn.connect_clicked(move |_| {
            settings.borrow_mut().keybinds.clear();
            settings.borrow().save();
            if let Some(gio_app) = gio::Application::default()
                && let Ok(app) = gio_app.downcast::<gtk::Application>() {
                    apply_all_accels(&app, &settings.borrow());
                }
            modified_count.set(0);
            update_reset_all_label(&reset_all_btn2, 0);
            *recording_id.borrow_mut() = None;
            for id in row_handles
                .borrow()
                .keys()
                .filter(|k| k.as_str() != "__reset_all__")
            {
                refresh_row(
                    id,
                    &row_handles.borrow(),
                    &settings.borrow(),
                    &recording_id,
                    Some(0),
                );
            }
        });
    }

    // Search entry + reset-all button in a toolbar row inside the header group
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.set_margin_top(8);

    let search = gtk::SearchEntry::new();
    search.set_hexpand(true);
    search.set_placeholder_text(Some("Search by action or key..."));
    toolbar.append(&search);
    toolbar.append(&reset_all_btn);

    header_group.add(&toolbar);
    page.add(&header_group);

    // -- One PreferencesGroup per action group ---------------------------------
    for group in ALL_ACTION_GROUPS {
        let pref_group = adw::PreferencesGroup::new();
        pref_group.set_title(group.label);

        for action in group.actions {
            let handles = build_action_row(
                action,
                &settings.borrow(),
                Rc::clone(&recording_id),
                Rc::clone(&row_handles),
                Rc::clone(&settings),
                Rc::clone(&modified_count),
            );
            if let Some(ref row) = handles.row {
                pref_group.add(row);
            }
            row_handles
                .borrow_mut()
                .insert(action.id.to_string(), handles);
        }

        page.add(&pref_group);
    }

    // Search filtering (wired after all rows are registered)
    {
        let row_handles = Rc::clone(&row_handles);
        let settings_rc = Rc::clone(&settings);
        search.connect_search_changed(move |entry| {
            let query = entry.text().to_lowercase();
            let handles = row_handles.borrow();
            let settings = settings_rc.borrow();

            for (action_id, handle) in handles.iter() {
                if action_id == "__reset_all__" {
                    continue;
                }
                let Some(ref row) = handle.row else { continue };

                if query.is_empty() {
                    row.set_visible(true);
                    continue;
                }

                let info = ALL_ACTION_GROUPS
                    .iter()
                    .flat_map(|g| g.actions)
                    .find(|a| a.id == action_id.as_str());
                let Some(info) = info else { continue };

                let label_match = info.label.to_lowercase().contains(&query);
                let accel_match = info
                    .resolve_accel(&settings)
                    .is_some_and(|accel| {
                        let parts = format_accel(accel);
                        // match "ctrl+s", "ctrls", or any individual part like "ctrl" or "s"
                        parts.join("+").to_lowercase().contains(&query)
                            || parts.join("").to_lowercase().contains(&query)
                            || parts.iter().any(|p| p.to_lowercase().contains(&query))
                    });

                row.set_visible(label_match || accel_match);
            }
        });
    }

    page
}

pub(super) fn build_action_row(
    action: &ActionInfo,
    settings: &AppSettings,
    recording_id: Rc<RefCell<Option<String>>>,
    row_handles: Rc<RefCell<HashMap<String, RowHandles>>>,
    settings_rc: Rc<RefCell<AppSettings>>,
    modified_count: Rc<Cell<u32>>,
) -> RowHandles {
    let row = adw::ActionRow::new();
    row.set_title(action.label);
    row.set_activatable(true);

    let current_accel = action.resolve_accel(settings);
    let is_modified = settings.keybinds.contains_key(action.id);

    // Key badge suffix box
    let key_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    key_box.set_valign(gtk::Align::Center);
    populate_key_box(&key_box, current_accel, false);

    // Per-row reset button (<-), visible only when modified
    let reset_btn = gtk::Button::from_icon_name("edit-undo-symbolic");
    reset_btn.add_css_class("flat");
    reset_btn.add_css_class("circular");
    reset_btn.set_valign(gtk::Align::Center);
    reset_btn.set_visible(is_modified);
    reset_btn.set_tooltip_text(Some("Reset to default"));

    row.add_suffix(&key_box);
    row.add_suffix(&reset_btn);

    // Show subtitle when modified
    if is_modified
        && let Some(default) = action.default_accel {
            row.set_subtitle(&format!("Default: {}", format_accel(default).join("+")));
        }

    // Row activation -> enter recording mode
    {
        let action_id = action.id.to_string();
        let recording_id = Rc::clone(&recording_id);
        let row_handles = Rc::clone(&row_handles);
        let settings_rc = Rc::clone(&settings_rc);
        row.connect_activated(move |_| {
            let currently_recording = recording_id.borrow().as_deref() == Some(&action_id);
            if currently_recording {
                // Cancel
                *recording_id.borrow_mut() = None;
            } else {
                // Cancel any previous recording row first
                if let Some(prev_id) = recording_id.borrow().clone() {
                    *recording_id.borrow_mut() = None;
                    refresh_row(
                        &prev_id,
                        &row_handles.borrow(),
                        &settings_rc.borrow(),
                        &recording_id,
                        None,
                    );
                }
                *recording_id.borrow_mut() = Some(action_id.clone());
            }
            refresh_row(
                &action_id,
                &row_handles.borrow(),
                &settings_rc.borrow(),
                &recording_id,
                None,
            );
        });
    }

    // Per-row reset button click
    {
        let action_id = action.id.to_string();
        let settings_rc = Rc::clone(&settings_rc);
        let row_handles = Rc::clone(&row_handles);
        let recording_id = Rc::clone(&recording_id);
        let modified_count = Rc::clone(&modified_count);
        reset_btn.connect_clicked(move |_| {
            settings_rc.borrow_mut().keybinds.remove(&action_id);
            settings_rc.borrow().save();
            if let Some(gio_app) = gio::Application::default()
                && let Ok(app) = gio_app.downcast::<gtk::Application>() {
                    apply_all_accels(&app, &settings_rc.borrow());
                }
            let new_count = count_modified(&settings_rc.borrow());
            modified_count.set(new_count);
            if let Some(h) = row_handles.borrow().get("__reset_all__") {
                update_reset_all_label(&h.reset_all_btn, new_count);
            }
            *recording_id.borrow_mut() = None;
            refresh_row(
                &action_id,
                &row_handles.borrow(),
                &settings_rc.borrow(),
                &recording_id,
                Some(new_count),
            );
        });
    }

    RowHandles {
        row: Some(row),
        key_suffix_box: key_box,
        reset_all_btn: gtk::Button::new(), // placeholder; real one stored under "__reset_all__"
        row_reset_btn: reset_btn,
    }
}

pub(super) fn refresh_row(
    action_id: &str,
    handles: &HashMap<String, RowHandles>,
    settings: &AppSettings,
    recording_id: &Rc<RefCell<Option<String>>>,
    new_modified_count: Option<u32>,
) {
    let Some(h) = handles.get(action_id) else {
        return;
    };
    let Some(ref row) = h.row else { return };

    let action_info = ALL_ACTION_GROUPS
        .iter()
        .flat_map(|g| g.actions)
        .find(|a| a.id == action_id);
    let Some(info) = action_info else { return };

    let is_recording = recording_id.borrow().as_deref() == Some(action_id);
    let is_modified = settings.keybinds.contains_key(action_id);
    let current_accel = info.resolve_accel(settings);

    // Recording CSS class
    if is_recording {
        row.add_css_class("keybind-recording-row");
    } else {
        row.remove_css_class("keybind-recording-row");
    }

    // Key badge suffix
    populate_key_box(&h.key_suffix_box, current_accel, is_recording);

    // Subtitle
    if is_recording {
        row.set_subtitle("Press a key combination...  (Esc to cancel . Backspace to unbind)");
    } else if is_modified {
        if let Some(default) = info.default_accel {
            row.set_subtitle(&format!("Default: {}", format_accel(default).join("+")));
        } else {
            row.set_subtitle("");
        }
    } else {
        row.set_subtitle("");
    }

    // Per-row reset button visibility
    h.row_reset_btn.set_visible(is_modified && !is_recording);

    // Update global reset-all button label if a count was provided
    if let Some(count) = new_modified_count
        && let Some(sentinel) = handles.get("__reset_all__") {
            update_reset_all_label(&sentinel.reset_all_btn, count);
        }
}

// Helpers

pub(super) fn populate_key_box(key_box: &gtk::Box, accel: Option<&str>, is_recording: bool) {
    while let Some(child) = key_box.first_child() {
        key_box.remove(&child);
    }

    if is_recording {
        // Show dashed placeholder box
        let placeholder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        placeholder.set_size_request(28, 28);
        placeholder.add_css_class("key-recording-placeholder");
        key_box.append(&placeholder);
        return;
    }

    if let Some(a) = accel {
        let parts = format_accel(a);
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                let sep = gtk::Label::new(Some("+"));
                sep.add_css_class("key-sep");
                key_box.append(&sep);
            }
            key_box.append(&key_badge(part));
        }
    } else {
        let lbl = gtk::Label::new(Some(" - "));
        lbl.add_css_class("dim-label");
        key_box.append(&lbl);
    }
}

pub(super) fn key_badge(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("key-badge");
    l
}

pub(super) fn count_modified(settings: &AppSettings) -> u32 {
    u32::try_from(settings.keybinds.len()).unwrap_or(u32::MAX)
}

pub(super) fn update_reset_all_label(btn: &gtk::Button, count: u32) {
    if count == 0 {
        btn.set_label("Reset all");
        btn.set_sensitive(false);
    } else {
        btn.set_label(&format!("Reset all ({count})"));
        btn.set_sensitive(true);
    }
}

pub(super) fn is_modifier_key(k: gdk::Key) -> bool {
    matches!(
        k,
        gdk::Key::Control_L
            | gdk::Key::Control_R
            | gdk::Key::Shift_L
            | gdk::Key::Shift_R
            | gdk::Key::Alt_L
            | gdk::Key::Alt_R
            | gdk::Key::Super_L
            | gdk::Key::Super_R
            | gdk::Key::Meta_L
            | gdk::Key::Meta_R
            | gdk::Key::Hyper_L
            | gdk::Key::Hyper_R
            | gdk::Key::Caps_Lock
            | gdk::Key::Num_Lock
            | gdk::Key::ISO_Level3_Shift
    )
}

/// Map a bare modifier key to its accel string, for modifier-only bindings
/// (canvas rotate / snap). Returns `None` for non-modifier or unsupported keys.
pub(super) fn modifier_only_accel(keyval: gdk::Key) -> Option<&'static str> {
    match keyval {
        gdk::Key::Shift_L | gdk::Key::Shift_R => Some("<Shift>"),
        gdk::Key::Control_L | gdk::Key::Control_R => Some("<Primary>"),
        gdk::Key::Alt_L | gdk::Key::Alt_R => Some("<Alt>"),
        _ => None,
    }
}

pub(super) fn build_accel_string(keyval: gdk::Key, state: gdk::ModifierType) -> String {
    let mut s = String::new();
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        s.push_str("<Primary>");
    }
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        s.push_str("<Shift>");
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        s.push_str("<Alt>");
    }
    if state.contains(gdk::ModifierType::SUPER_MASK) {
        s.push_str("<Super>");
    }
    if let Some(name) = keyval.name() {
        let n = name.as_str();
        if n.len() == 1 && n.chars().next().is_some_and(char::is_alphabetic) {
            s.push_str(&n.to_lowercase());
        } else {
            s.push_str(n);
        }
    }
    s
}

// CSS
