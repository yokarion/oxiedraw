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

/// How a write behaves: toasts, backups, and how the saved state updates.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveKind {
    /// Ctrl+S / Save As: toasts, backups, records the path.
    Manual,
    /// Autosave to the document's file: silent, no backups, marks it saved.
    Autosave,
    /// Autosave of an untitled document to its recovery copy: silent, no
    /// backups, and leaves it marked unsaved (it still has no file).
    Recovery,
}

fn do_save(session: &Rc<DocumentSession>, window: &adw::ApplicationWindow, path: &Path) {
    let backups = crate::settings::AppSettings::load().save.effective_backup_count();
    write_project(
        session,
        window,
        path.to_path_buf(),
        SaveKind::Manual,
        backups,
        Rc::new(|| {}),
    );
}

/// Snapshot the canvas and write it to `path` on a worker thread. `on_done`
/// runs on the main thread once the write settles (success or not), letting
/// autosave chain the next document.
fn write_project(
    session: &Rc<DocumentSession>,
    window: &adw::ApplicationWindow,
    path: PathBuf,
    kind: SaveKind,
    backup_count: usize,
    on_done: Rc<dyn Fn()>,
) {
    // Can't serialize mid component-edit (wrong layers on the canvas), and only
    // one write at a time.
    if session.is_editing_component() || session.global.save_in_progress.get() {
        on_done();
        return;
    }

    // A liquify session holds the in-flight stroke in a GPU field over a
    // snapshot, so the layer still has its pre-warp pixels until the stroke
    // bakes at pen-up. Close the session here - the shared choke point for every
    // writer - so autosave and the untitled-document recovery copy can't fire
    // between motion events and persist an image missing the live stroke.
    (session.liquify_flush)();

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
        let gradient = session.gradient.settings.borrow().clone();
        let view_rotation = session.viewport.rotation();
        let guide = session.guide.config.borrow().clone();
        match project::save::snapshot(
            &mut canvas.borrow_mut(),
            &props,
            &components,
            &fonts,
            gradient,
            view_rotation,
            guide,
        ) {
            Ok(s) => s,
            Err(e) => {
                if kind == SaveKind::Manual {
                    show_error(window, "Save Failed", &e.to_string());
                } else {
                    tracing::warn!(err = %e, "autosave snapshot failed");
                }
                on_done();
                return;
            }
        }
    };

    session.global.save_in_progress.set(true);
    let pending = (kind == SaveKind::Manual)
        .then(|| session.global.toaster.pending("Saving project..."));

    // Phase 2 (worker thread): PNG-encode + write the TAR archive.
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let path_for_worker = path.clone();
    std::thread::spawn(move || {
        let result = project::save::write_snapshot(&snapshot, &path_for_worker, backup_count)
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });

    // Poll for completion on the main thread (no async runtime in this app).
    let session = Rc::clone(session);
    let window = window.clone();
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
        match (outcome, kind) {
            (Ok(()), SaveKind::Manual) => {
                tracing::info!(path = %path.display(), "project saved");
                *session.file_path.borrow_mut() = Some(path.clone());
                if let Some(stem) = path.file_stem() {
                    *session.title.borrow_mut() = stem.to_string_lossy().into_owned();
                }
                session.mark_saved();
                session.refresh_tab_title();
                // The document now lives in a real file; drop any recovery copy.
                session.clear_recovery();

                session.global.toaster.info("Project saved!");
            }
            (Ok(()), SaveKind::Autosave) => {
                tracing::debug!(path = %path.display(), "autosaved project");
                session.mark_saved();
                session.refresh_tab_title();
            }
            (Ok(()), SaveKind::Recovery) => {
                tracing::debug!(path = %path.display(), "wrote recovery autosave");
            }
            (Err(e), SaveKind::Manual) => show_error(&window, "Save Failed", &e),
            (Err(e), _) => tracing::warn!(err = %e, "autosave failed"),
        }
        on_done();
        glib::ControlFlow::Break
    });
}

/// Autosave every open document with unsaved changes. Saved docs go back to
/// their file; untitled docs go to a recovery copy. Writes run one at a time,
/// chained through each write's completion callback.
pub(crate) fn autosave_all(
    sessions: Vec<Rc<DocumentSession>>,
    window: adw::ApplicationWindow,
) {
    let mut queue: Vec<(Rc<DocumentSession>, PathBuf, SaveKind)> = Vec::new();
    for session in sessions {
        let has_path = session.file_path.borrow().is_some();
        match autosave_action(
            session.is_editing_component(),
            has_path,
            session.is_dirty(),
            session.change_counter(),
            session.last_autosave_len.get(),
        ) {
            AutosaveAction::None => {}
            AutosaveAction::ToPath => {
                let path = session.file_path.borrow().clone().expect("path checked above");
                queue.push((session, path, SaveKind::Autosave));
            }
            AutosaveAction::ToRecovery => {
                if let Some(path) = session.ensure_recovery_path() {
                    session.last_autosave_len.set(Some(session.change_counter()));
                    queue.push((session, path, SaveKind::Recovery));
                }
            }
        }
    }
    process_autosave_queue(Rc::new(queue), 0, window);
}

/// What an autosave pass should do with a single document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutosaveAction {
    /// Leave it alone (unchanged, mid-component-edit, or an empty untitled doc).
    None,
    /// Save silently to the document's own file.
    ToPath,
    /// Save silently to a recovery copy (the document has no file yet).
    ToRecovery,
}

/// Decide how to autosave one document. Split out from [`autosave_all`] so the
/// rules are testable without a live session: never mid component edit, saved
/// docs only when dirty, untitled docs only once they have content and changed
/// since the last recovery write.
fn autosave_action(
    editing_component: bool,
    has_path: bool,
    is_dirty: bool,
    change_counter: usize,
    last_autosave_len: Option<usize>,
) -> AutosaveAction {
    if editing_component {
        return AutosaveAction::None;
    }
    if has_path {
        if is_dirty {
            AutosaveAction::ToPath
        } else {
            AutosaveAction::None
        }
    } else if change_counter > 0 && last_autosave_len != Some(change_counter) {
        AutosaveAction::ToRecovery
    } else {
        AutosaveAction::None
    }
}

fn process_autosave_queue(
    queue: Rc<Vec<(Rc<DocumentSession>, PathBuf, SaveKind)>>,
    index: usize,
    window: adw::ApplicationWindow,
) {
    let Some((session, path, kind)) = queue.get(index).cloned() else {
        return;
    };
    let next: Rc<dyn Fn()> = {
        let queue = Rc::clone(&queue);
        let window = window.clone();
        Rc::new(move || process_autosave_queue(Rc::clone(&queue), index + 1, window.clone()))
    };
    // Autosave never rotates the numbered backups (those are for manual saves).
    write_project(&session, &window, path, kind, 0, next);
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

#[cfg(test)]
mod tests {
    use super::{AutosaveAction, autosave_action};

    // Never autosave while editing a component, whatever else is true.
    #[test]
    fn skips_while_editing_a_component() {
        assert_eq!(
            autosave_action(true, true, true, 5, None),
            AutosaveAction::None
        );
        assert_eq!(
            autosave_action(true, false, true, 5, Some(1)),
            AutosaveAction::None
        );
    }

    #[test]
    fn saved_document_writes_back_only_when_dirty() {
        assert_eq!(
            autosave_action(false, true, true, 9, None),
            AutosaveAction::ToPath
        );
        assert_eq!(
            autosave_action(false, true, false, 9, None),
            AutosaveAction::None,
            "a clean document has nothing to autosave"
        );
    }

    #[test]
    fn untitled_document_goes_to_recovery_once_it_has_content() {
        // Fresh empty untitled doc (no edits yet) - nothing worth recovering.
        assert_eq!(
            autosave_action(false, false, true, 0, None),
            AutosaveAction::None
        );
        // After the first edit, write a recovery copy.
        assert_eq!(
            autosave_action(false, false, true, 1, None),
            AutosaveAction::ToRecovery
        );
    }

    #[test]
    fn untitled_document_is_not_rewritten_when_unchanged() {
        // Already recovered at change-counter 4, still at 4 - skip.
        assert_eq!(
            autosave_action(false, false, true, 4, Some(4)),
            AutosaveAction::None
        );
        // Edited again (counter moved to 5) - recover the new state.
        assert_eq!(
            autosave_action(false, false, true, 5, Some(4)),
            AutosaveAction::ToRecovery
        );
    }
}
