//! Manage Brushes window.
//!
//! Two-pane libadwaita window opened via the `app.brush-manager`
//! action. Left pane lists every loaded brush; right pane shows the
//! editor (`editor.rs`) which writes back to the in-memory brush and
//! debounces a `format::save` to disk; the file watcher then triggers
//! reload, and the manager's `brushes_changed` listener re-selects
//! the same brush by name.
//!
//! Header (left pane) menu: Re-generate built-in brushes, Open
//! brushes folder. Delete brush is on the right pane.

mod editor;
mod preview;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::brush_engine::{
    BrushEngine, BrushPreset, BrushPresetId, BrushRegistry, builtins, format,
};
use relm4::gtk;

use crate::brush_picker::shared as picker;

const WINDOW_WIDTH: i32 = 980;
const WINDOW_HEIGHT: i32 = 680;
const SIDEBAR_WIDTH: i32 = 260;

/// Open the Manage Brushes window. Modal to `parent`.
pub(crate) fn show(
    parent: &adw::ApplicationWindow,
    brush_engine: &BrushEngine,
    default_brush_name: std::rc::Rc<std::cell::RefCell<Option<String>>>,
) {
    let win = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(WINDOW_WIDTH)
        .default_height(WINDOW_HEIGHT)
        .title("Manage Brushes")
        .build();
    let toast_overlay = adw::ToastOverlay::new();

    // Selected brush id + name. Both shared between left list, right
    // pane, and the brushes-changed listener so a click on row B
    // updates the name the listener uses to re-select after engine
    // reload assigns new ids.
    let selected_id: Rc<Cell<Option<BrushPresetId>>> = Rc::new(Cell::new(None));
    let selected_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Right pane: editor + handles for set/focus.
    let (right_pane, editor_handles) = editor::build(
        win.clone().upcast(),
        brush_engine.clone(),
        selected_id.clone(),
        selected_name.clone(),
    );
    let set_right = editor_handles.set_brush.clone();
    let focus_name = editor_handles.focus_name.clone();

    // Left pane: brush list + setter callback for re-render.
    let (left_pane, rebuild_left) = build_left_pane(
        win.clone().upcast(),
        brush_engine.clone(),
        selected_id.clone(),
        selected_name.clone(),
        set_right.clone(),
        focus_name.clone(),
        default_brush_name,
        toast_overlay.clone(),
    );

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&left_pane)
        .end_child(&right_pane)
        .resize_start_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(SIDEBAR_WIDTH)
        .build();

    toast_overlay.set_child(Some(&paned));
    win.set_content(Some(&toast_overlay));

    // Initial selection: active brush, else first.
    {
        let brushes = brush_engine.brushes.borrow();
        let initial = brushes
            .iter()
            .find(|p| p.id == brush_engine.active.get())
            .or_else(|| brushes.first());
        let initial_id = initial.map(|p| p.id);
        selected_id.set(initial_id);
        *selected_name.borrow_mut() = initial.map(|p| p.name.clone());
        if let Some(preset) = initial {
            set_right(Some(preset));
        }
    }
    rebuild_left();

    // Refresh list + right pane when the engine reloads brushes (e.g.
    // autoreload after a save). The listener is detached on window
    // close so reopens don't pile up dead UI references.
    let listener_id = {
        let rebuild_left = rebuild_left.clone();
        let set_right = set_right.clone();
        let brush_engine_for_cb = brush_engine.clone();
        let selected_id_for_cb = selected_id.clone();
        let selected_name_for_cb = selected_name.clone();
        brush_engine.connect_brushes_changed(Rc::new(move || {
            // Resolve previously-selected brush by *name* - engine
            // reload assigns new ids so the stale `selected_id` is no
            // help.
            let brushes = brush_engine_for_cb.brushes.borrow();
            let want_name = selected_name_for_cb.borrow().clone();
            let new_target = want_name
                .as_deref()
                .and_then(|name| brushes.iter().find(|p| p.name == name))
                .or_else(|| brushes.first());
            let new_id = new_target.map(|p| p.id);
            selected_id_for_cb.set(new_id);
            set_right(new_target);
            // Keep the name in sync - covers the unlikely case where
            // the brush vanished and we fell back to `brushes.first()`.
            *selected_name_for_cb.borrow_mut() = new_target.map(|p| p.name.clone());
            drop(brushes);
            rebuild_left();
        }))
    };

    {
        let brush_engine_for_close = brush_engine.clone();
        win.connect_close_request(move |_| {
            brush_engine_for_close.disconnect_brushes_changed(listener_id);
            relm4::gtk::glib::Propagation::Proceed
        });
    }

    win.present();
}

// ---------------------------------------------------------------------------
// Left pane
// ---------------------------------------------------------------------------

fn build_left_pane(
    parent: gtk::Window,
    brush_engine: BrushEngine,
    selected_id: Rc<Cell<Option<BrushPresetId>>>,
    selected_name: Rc<RefCell<Option<String>>>,
    set_right: Rc<dyn Fn(Option<&BrushPreset>)>,
    focus_name: Rc<dyn Fn()>,
    default_brush_name: Rc<RefCell<Option<String>>>,
    toast_overlay: adw::ToastOverlay,
) -> (gtk::Widget, Rc<dyn Fn()>) {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    outer.add_css_class("sidebar");

    // Sidebar header - full-height adw::HeaderBar so the sidebar
    // column extends to the top of the window frame.
    let title = gtk::Label::builder()
        .label("Manage Brushes")
        .build();
    title.add_css_class("heading");
    let header = adw::HeaderBar::builder()
        .title_widget(&title)
        .show_end_title_buttons(false)
        .build();

    // "+" button to create a new brush.
    let add_button = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .has_frame(false)
        .tooltip_text("New brush")
        .build();
    {
        let brush_engine = brush_engine.clone();
        let selected_id = selected_id.clone();
        let selected_name = selected_name.clone();
        let set_right = set_right.clone();
        let focus_name = focus_name.clone();
        let parent_for_btn = parent.clone();
        add_button.connect_clicked(move |_| {
            // `selected_name` must be set *before* `add_brush` runs  - 
            // it fires the brushes-changed listener synchronously, and
            // the listener re-selects by name. Setting it after would
            // leave the list highlighting the previously-selected
            // brush until the next watcher reload.
            match create_new_brush(&brush_engine, &selected_id, &selected_name) {
                Ok(new_id) => {
                    let brushes = brush_engine.brushes.borrow();
                    if let Some(p) = brushes.iter().find(|p| p.id == new_id) {
                        set_right(Some(p));
                    }
                    drop(brushes);
                    focus_name();
                }
                Err(e) => {
                    tracing::warn!(%e, "failed to create new brush");
                    show_simple_error(
                        &parent_for_btn,
                        "Couldn't create new brush",
                        &e.to_string(),
                    );
                }
            }
        });
    }
    header.pack_end(&add_button);

    // Hamburger menu: Reload / Re-generate / Open folder.
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .has_frame(false)
        .build();
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Reload brush library"), Some("app.brush-reload"));
    menu.append(Some("Re-generate built-in brushes"), Some("app.brush-regen"));
    menu.append(Some("Open brushes folder"), Some("app.brush-open-folder"));
    menu_button.set_menu_model(Some(&menu));
    header.pack_end(&menu_button);
    outer.append(&header);

    // Brush list.
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();
    let listbox = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .build();
    listbox.add_css_class("navigation-sidebar");
    scrolled.set_child(Some(&listbox));
    outer.append(&scrolled);

    let row_map: Rc<RefCell<Vec<(BrushPresetId, gtk::ListBoxRow)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let star_map: Rc<RefCell<Vec<(BrushPresetId, gtk::Button)>>> =
        Rc::new(RefCell::new(Vec::new()));

    let rebuild: Rc<dyn Fn()> = {
        let brush_engine = brush_engine.clone();
        let listbox = listbox.clone();
        let row_map = row_map.clone();
        let star_map = star_map.clone();
        let selected_id = selected_id.clone();
        let default_brush_name = default_brush_name.clone();
        let toast_overlay = toast_overlay.clone();
        Rc::new(move || {
            while let Some(child) = listbox.first_child() {
                listbox.remove(&child);
            }
            row_map.borrow_mut().clear();
            star_map.borrow_mut().clear();
            let brushes = brush_engine.brushes.borrow();
            let sel = selected_id.get();
            let def_name = default_brush_name.borrow().clone();
            let mut row_to_select: Option<gtk::ListBoxRow> = None;
            for preset in brushes.iter() {
                let is_default = def_name.as_deref() == Some(preset.name.as_str());
                let preset_name = preset.name.clone();
                let brush_id = preset.id;
                let default_brush_name_c = default_brush_name.clone();
                let toast_overlay_c = toast_overlay.clone();
                let star_map_c = star_map.clone();
                let on_set_default: Rc<dyn Fn()> = Rc::new(move || {
                    *default_brush_name_c.borrow_mut() = Some(preset_name.clone());
                    let mut settings = crate::settings::AppSettings::load();
                    settings.default_brush_name = Some(preset_name.clone());
                    settings.save();
                    let toast = adw::Toast::new(&format!("Default brush changed to: {preset_name}"));
                    toast_overlay_c.add_toast(toast);
                    for (id, btn) in star_map_c.borrow().iter() {
                        picker::update_star_icon(btn, *id == brush_id);
                    }
                });
                let (row, star_btn) = picker::build_list_row(preset, is_default, Some(on_set_default));
                if let Some(btn) = star_btn {
                    star_map.borrow_mut().push((preset.id, btn));
                }
                listbox.append(&row);
                if Some(preset.id) == sel {
                    row_to_select = Some(row.clone());
                }
                row_map.borrow_mut().push((preset.id, row));
            }
            if let Some(row) = row_to_select {
                listbox.select_row(Some(&row));
            }
        })
    };

    {
        let brush_engine = brush_engine.clone();
        let row_map = row_map.clone();
        let selected_id = selected_id.clone();
        let selected_name = selected_name.clone();
        let set_right = set_right.clone();
        listbox.connect_row_activated(move |_, row| {
            if let Some((id, _)) = row_map.borrow().iter().find(|(_, r)| r == row) {
                selected_id.set(Some(*id));
                let brushes = brush_engine.brushes.borrow();
                let preset = brushes.iter().find(|p| p.id == *id);
                // Track the name too so the brushes-changed listener
                // can re-select correctly after engine reload mints
                // fresh ids.
                *selected_name.borrow_mut() = preset.map(|p| p.name.clone());
                set_right(preset);
            }
        });
    }

    // Wire the sidebar's menu actions, scoped to this window's lifetime.
    register_window_actions(&parent, &brush_engine);

    (outer.upcast(), rebuild)
}

// ---------------------------------------------------------------------------
// Shared destructive/file ops - called from `editor.rs`.
// ---------------------------------------------------------------------------

pub(super) fn confirm_and_delete(
    parent: &gtk::Window,
    brush_engine: &BrushEngine,
    id: BrushPresetId,
) {
    // Look up the file path.
    let path: Option<PathBuf> = brush_engine
        .brushes
        .borrow()
        .iter()
        .find(|p| p.id == id)
        .and_then(|p| p.source_path.clone());
    let Some(path) = path else {
        // In-memory only (fallback brush). Nothing to delete on disk.
        tracing::info!("delete brush: no source path; skipping");
        return;
    };
    let detail = format!(
        "Permanently remove {}? This cannot be undone.",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("this brush")
    );
    let dialog = gtk::AlertDialog::builder()
        .message("Delete Brush")
        .detail(detail)
        .modal(true)
        .build();
    dialog.set_buttons(&["Cancel", "Delete"]);
    dialog.set_default_button(0);
    dialog.set_cancel_button(0);
    let path_for_cb = path.clone();
    dialog.choose(
        Some(parent),
        None::<&gtk::gio::Cancellable>,
        move |result| {
            if result == Ok(1) {
                if let Err(e) = std::fs::remove_file(&path_for_cb) {
                    tracing::warn!(?path_for_cb, %e, "failed to delete brush file");
                } else {
                    tracing::info!(?path_for_cb, "deleted brush");
                }
            }
        },
    );
}

pub(super) fn choose_icon(
    parent: &gtk::Window,
    brush_engine: &BrushEngine,
    id: BrushPresetId,
) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Images (PNG, JPEG)"));
    filter.add_mime_type("image/png");
    filter.add_mime_type("image/jpeg");
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    let dialog = gtk::FileDialog::builder()
        .title("Choose brush icon")
        .modal(true)
        .filters(&filters)
        .build();

    let brush_engine = brush_engine.clone();
    let parent_for_cb = parent.clone();
    dialog.open(
        Some(parent),
        gtk::gio::Cancellable::NONE,
        move |result| {
            let Ok(file) = result else { return };
            let Some(path) = file.path() else { return };
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(?path, %e, "failed to read icon file");
                    return;
                }
            };
            if let Err(e) = apply_icon_to_brush(&brush_engine, id, bytes) {
                tracing::warn!(%e, "failed to apply icon");
                show_simple_error(&parent_for_cb, "Couldn't update icon", &e.to_string());
            }
        },
    );
}

fn apply_icon_to_brush(
    brush_engine: &BrushEngine,
    id: BrushPresetId,
    icon_bytes: Vec<u8>,
) -> Result<(), format::BrushError> {
    // Build an updated preset off the current in-memory copy.
    let mut updated = brush_engine
        .brushes
        .borrow()
        .iter()
        .find(|p| p.id == id)
        .cloned()
        .ok_or_else(|| {
            format::BrushError::MissingEntry("brush id not found in engine".to_string())
        })?;
    updated.icon = Some(icon_bytes);
    // Write to disk if we know where it came from; the file watcher
    // will pick up the change and rebuild engine state.
    let target_path = if let Some(p) = &updated.source_path { p.clone() } else {
        // In-memory only - derive a path in the user dir.
        let dir = BrushRegistry::config_dir().ok_or_else(|| {
            format::BrushError::MissingEntry("XDG config dir not resolvable".to_string())
        })?;
        std::fs::create_dir_all(&dir)?;
        dir.join(format!("{}.oxiebrush", sanitize_filename(&updated.name)))
    };
    format::save(&updated, &target_path)?;
    Ok(())
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn show_simple_error(parent: &gtk::Window, heading: &str, body: &str) {
    let dialog = gtk::AlertDialog::builder()
        .message(heading)
        .detail(body)
        .modal(true)
        .build();
    dialog.set_buttons(&["OK"]);
    dialog.choose(Some(parent), None::<&gtk::gio::Cancellable>, |_| {});
}

// ---------------------------------------------------------------------------
// Create new brush
// ---------------------------------------------------------------------------

/// Build a fresh `BrushPreset` with sensible defaults, ensure its
/// display name + filename are unique in the user dir, write it to
/// disk, and append it to the engine's brush list. Pre-sets the
/// shared `selected_name` so the brushes-changed listener that
/// `add_brush` triggers re-selects the new brush correctly.
fn create_new_brush(
    brush_engine: &BrushEngine,
    selected_id: &Rc<Cell<Option<BrushPresetId>>>,
    selected_name: &Rc<RefCell<Option<String>>>,
) -> Result<BrushPresetId, format::BrushError> {
    let dir = BrushRegistry::config_dir().ok_or_else(|| {
        format::BrushError::MissingEntry("XDG config dir not resolvable".to_string())
    })?;
    std::fs::create_dir_all(&dir)?;

    let (name, path) = pick_unique_name_and_path(brush_engine, &dir);
    *selected_name.borrow_mut() = Some(name.clone());
    let preset = BrushPreset {
        id: BrushPresetId(0),
        name,
        family: oxiedraw_core::brush_engine::BrushFamily::SoftRound,
        default_size: 12.0,
        default_opacity: 1.0,
        spacing_ratio: 0.1,
        stabilizer: 0.0,
        speed_smoothing: 0.0,
        buildup: false,
        hardness: 1.0,
        tip: oxiedraw_core::brush_engine::TipShape::Round,
        texture_scale: 0.0,
        texture_strength: 0.0,
        dynamics: oxiedraw_core::brush_engine::Dynamics::default(),
        icon: None,
        preview: None,
        source_path: Some(path.clone()),
    };
    format::save(&preset, &path)?;
    let new_id = brush_engine.add_brush(preset);
    selected_id.set(Some(new_id));
    Ok(new_id)
}

fn pick_unique_name_and_path(
    brush_engine: &BrushEngine,
    dir: &std::path::Path,
) -> (String, PathBuf) {
    let used_names: std::collections::HashSet<String> = brush_engine
        .brushes
        .borrow()
        .iter()
        .map(|p| p.name.clone())
        .collect();
    for i in 1..1024 {
        let name = if i == 1 {
            "New Brush".to_string()
        } else {
            format!("New Brush {i}")
        };
        let stem = sanitize_filename(&name);
        let path = dir.join(format!("{stem}.oxiebrush"));
        if !used_names.contains(&name) && !path.exists() {
            return (name, path);
        }
    }
    // Fallback - extremely unlikely to be reached.
    let path = dir.join("new_brush.oxiebrush");
    ("New Brush".to_string(), path)
}

// ---------------------------------------------------------------------------
// Window-scoped actions: Re-generate, Open folder
// ---------------------------------------------------------------------------

fn register_window_actions(parent: &gtk::Window, brush_engine: &BrushEngine) {
    let group = gtk::gio::SimpleActionGroup::new();

    // Reload the on-disk brush library. The file watcher used to do
    // this automatically, but its reload-and-reapply race with
    // in-flight slider drags was clobbering edits - this is the manual
    // replacement. Any unsaved in-memory edits (pending save timer)
    // are lost: reload reads the disk authoritatively.
    let reload = gtk::gio::SimpleAction::new("brush-reload", None);
    let parent_for_reload = parent.clone();
    let brush_engine_for_reload = brush_engine.clone();
    reload.connect_activate(move |_, _| {
        let Some(dir) = BrushRegistry::config_dir() else {
            show_simple_error(
                &parent_for_reload,
                "Brushes folder unavailable",
                "Couldn't resolve $XDG_CONFIG_HOME for the brushes directory.",
            );
            return;
        };
        brush_engine_for_reload.reload_from_dir(&dir);
        brush_engine_for_reload.backfill_missing_previews();
    });
    group.add_action(&reload);

    let regen = gtk::gio::SimpleAction::new("brush-regen", None);
    let parent_for_regen = parent.clone();
    regen.connect_activate(move |_, _| {
        let Some(dir) = BrushRegistry::config_dir() else {
            show_simple_error(
                &parent_for_regen,
                "Brushes folder unavailable",
                "Couldn't resolve $XDG_CONFIG_HOME for the brushes directory.",
            );
            return;
        };
        match builtins::seed_all(&dir) {
            Ok(n) => tracing::info!(count = n, "re-generated built-in brushes"),
            Err(e) => {
                tracing::warn!(%e, "re-generate failed");
                show_simple_error(
                    &parent_for_regen,
                    "Couldn't re-generate built-in brushes",
                    &e.to_string(),
                );
            }
        }
    });
    group.add_action(&regen);

    let open = gtk::gio::SimpleAction::new("brush-open-folder", None);
    let parent_for_open = parent.clone();
    open.connect_activate(move |_, _| {
        let Some(dir) = BrushRegistry::config_dir() else {
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(?dir, %e, "failed to create brushes dir");
        }
        let file = gtk::gio::File::for_path(&dir);
        let launcher = gtk::FileLauncher::new(Some(&file));
        launcher.launch(
            Some(&parent_for_open),
            gtk::gio::Cancellable::NONE,
            move |result| {
                if let Err(e) = result {
                    tracing::warn!(%e, "failed to launch brushes folder");
                }
            },
        );
    });
    group.add_action(&open);

    parent.insert_action_group("app", Some(&group));
}

