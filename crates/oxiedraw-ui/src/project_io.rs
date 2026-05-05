//! Project save/open dialogs, operating on a [`DocumentSession`].
//!
//! Save targets the active document (writing to its existing path, or prompting
//! when there is none / on "Save As"). Open loads a project and hands it back so
//! the caller can spin up a fresh tab for it.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gio;
use gtk::glib;

use oxiedraw_core::project::{self, format::OxieProject};

use crate::session::DocumentSession;

/// Save `session`. When it already has a path and `force_dialog` is false, write
/// straight to that path; otherwise prompt for a location ("Save As").
pub(crate) fn save(
    session: &Rc<DocumentSession>,
    window: &adw::ApplicationWindow,
    force_dialog: bool,
) {
    // While a component is open in edit mode the canvas holds the component's
    // layers, not the document's - saving now would serialize the wrong thing.
    if session.is_editing_component() {
        session
            .global
            .toaster
            .info("Finish editing the component before saving.");
        return;
    }

    // Only one disk write at a time across all documents.
    if session.global.save_in_progress.get() {
        session
            .global
            .toaster
            .info("Saving project task is already in progress!");
        return;
    }

    let existing = session.file_path.borrow().clone();
    if let (false, Some(path)) = (force_dialog, existing) {
        do_save(session, window, &path);
        return;
    }

    let filters = project_file_filters();
    let dialog = gtk::FileDialog::new();
    dialog.set_title("Save Project");
    dialog.set_modal(true);
    dialog.set_filters(Some(&filters));
    let initial = format!("{}.oxiedrawproj", session.title.borrow());
    dialog.set_initial_name(Some(&initial));

    let session = Rc::clone(session);
    let window_cb = window.clone();
    dialog.save(Some(window), None::<&gio::Cancellable>, move |result| {
        let Ok(file) = result else { return };
        let Some(path) = file.path() else { return };
        let path = if path.extension().is_none_or(|e| e != "oxiedrawproj") {
            path.with_extension("oxiedrawproj")
        } else {
            path
        };
        do_save(&session, &window_cb, &path);
    });
}

/// The set of font families referenced by any text layer in the canvas.
fn used_text_families(canvas: &oxiedraw_core::canvas::Canvas) -> std::collections::HashSet<String> {
    let mut families = std::collections::HashSet::new();
    for layer in canvas.layers().snapshot() {
        if let Some(content) = layer.text_content() {
            families.insert(content.default_style.font.0.clone());
            for run in &content.runs {
                families.insert(run.style.font.0.clone());
            }
        }
    }
    families
}

fn do_save(session: &Rc<DocumentSession>, window: &adw::ApplicationWindow, path: &Path) {
    // Phase 1 (main thread): read the layers back from the GPU into a Send-able
    // snapshot. This is the only part that needs the Vulkan canvas.
    let props = session.current_properties();
    let snapshot = {
        let canvas = session.viewport.canvas();
        let components = session.components.borrow();
        // Embed the font files used by any text layer so the project renders
        // (and stays editable) on machines without those fonts installed.
        let fonts = {
            let families = used_text_families(&canvas.borrow());
            session.global.text_engine.borrow().embed_used_fonts(&families)
        };
        match project::save::snapshot(&mut canvas.borrow_mut(), &props, &components, &fonts) {
            Ok(s) => s,
            Err(e) => {
                show_error(window, "Save Failed", &e.to_string());
                return;
            }
        }
    };

    session.global.save_in_progress.set(true);
    let pending = session.global.toaster.pending("Saving project...");

    // Phase 2 (worker thread): PNG-encode + write the TAR archive.
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let path_for_worker = path.to_path_buf();
    std::thread::spawn(move || {
        let result = project::save::write_snapshot(&snapshot, &path_for_worker).map_err(|e| e.to_string());
        let _ = tx.send(result);
    });

    // Poll for completion on the main thread (no async runtime in this app).
    let session = Rc::clone(session);
    let window = window.clone();
    let path = path.to_path_buf();
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let outcome = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("the save worker terminated unexpectedly".to_string())
            }
        };

        session.global.save_in_progress.set(false);
        if let Some(t) = pending.as_ref() {
            t.dismiss();
        }
        match outcome {
            Ok(()) => {
                tracing::info!(path = %path.display(), "project saved");
                *session.file_path.borrow_mut() = Some(path.clone());
                if let Some(stem) = path.file_stem() {
                    *session.title.borrow_mut() = stem.to_string_lossy().into_owned();
                }
                session.mark_saved();
                session.refresh_tab_title();

                let win = window.clone();
                let saved_path = path.clone();
                session
                    .global
                    .toaster
                    .action("Project saved!", "Open", move || {
                        open_containing_folder(&win, &saved_path);
                    });
            }
            Err(e) => show_error(&window, "Save Failed", &e),
        }
        glib::ControlFlow::Break
    });
}

/// Reveal a saved file in the system file manager.
fn open_containing_folder(window: &adw::ApplicationWindow, path: &Path) {
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    launcher.open_containing_folder(Some(window), None::<&gio::Cancellable>, |res| {
        if let Err(e) = res {
            tracing::warn!(error = %e, "open containing folder failed");
        }
    });
}

/// Prompt for a project file and load it. On success `on_loaded` is called with
/// the parsed project and its path so the caller can open it in a new tab.
pub(crate) fn open_dialog(
    window: &adw::ApplicationWindow,
    on_loaded: Rc<dyn Fn(OxieProject, PathBuf)>,
) {
    let filters = project_file_filters();
    let dialog = gtk::FileDialog::new();
    dialog.set_title("Open Project");
    dialog.set_modal(true);
    dialog.set_filters(Some(&filters));

    let window_cb = window.clone();
    dialog.open(Some(window), None::<&gio::Cancellable>, move |result| {
        let Ok(file) = result else { return };
        let Some(path) = file.path() else { return };
        match project::load::load(&path) {
            Ok(p) => on_loaded(p, path),
            Err(e) => show_error(&window_cb, "Open Failed", &e.to_string()),
        }
    });
}

fn project_file_filters() -> gio::ListStore {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("OxieDraw Project"));
    filter.add_pattern("*.oxiedrawproj");
    filter.add_mime_type("application/vnd.oxiedraw.project");

    let store = gio::ListStore::new::<gtk::FileFilter>();
    store.append(&filter);
    store
}

pub(crate) fn show_error(window: &adw::ApplicationWindow, heading: &str, body: &str) {
    let dialog = gtk::AlertDialog::builder()
        .message(heading)
        .detail(body)
        .build();
    dialog.set_buttons(&["OK"]);
    dialog.choose(Some(window), None::<&gio::Cancellable>, |_| {});
}
