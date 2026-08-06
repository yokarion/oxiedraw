//! Tab/document lifecycle.
//!
//! [`TabManager`] owns the open [`DocumentSession`]s, swaps the active
//! document's tool-options bar and right sidebar into the window's slot
//! containers when the selected tab changes, and routes the File-menu
//! operations (New / Open / Save / Save As / Close / Quit) to the active tab.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::gio;
use oxiedraw_core::project::{self, format::OxieProject};
use oxiedraw_core::tools::Tool;
use oxiedraw_utils::geometry::Size;
use relm4::gtk;
use relm4::gtk::glib;

use crate::canvas::Viewport;
use crate::session::{DocumentSession, GlobalState, SetActiveToolSlot};

pub(crate) struct TabManager {
    pub(crate) global: GlobalState,
    pub(crate) set_active_tool_late: SetActiveToolSlot,
    pub(crate) set_left_bar: Rc<dyn Fn(Tool)>,
    pub(crate) tab_view: adw::TabView,
    pub(crate) tool_options_slot: gtk::Box,
    pub(crate) right_bar_slot: gtk::Box,
    pub(crate) sessions: RefCell<Vec<Rc<DocumentSession>>>,
    pub(crate) active: RefCell<Option<Rc<DocumentSession>>>,
    pub(crate) root: adw::ApplicationWindow,
    pub(crate) history_capacity: usize,
    pub(crate) untitled_counter: Cell<u32>,
    /// When the last autosave ran; the tick measures the interval against it.
    pub(crate) last_autosave: Cell<Instant>,
}

impl TabManager {
    pub(crate) fn active(&self) -> Option<Rc<DocumentSession>> {
        self.active.borrow().clone()
    }

    /// Resolver for the currently focused viewport (used by zoom actions).
    pub(crate) fn active_viewport_provider(self: &Rc<Self>) -> Rc<dyn Fn() -> Option<Viewport>> {
        let manager = Rc::clone(self);
        Rc::new(move || manager.active().map(|s| s.viewport.clone()))
    }

    /// Window-level "set the active tool": updates the shared tool state, the
    /// left toolbar toggle, and runs the active document's tool-apply logic.
    pub(crate) fn set_active_tool(&self, t: Tool) {
        // Leaving a text edit (or any tool switch) commits the in-flight box.
        if let Some(s) = self.active.borrow().as_ref() {
            s.text_edit.commit();
        }
        self.global.tools.active.set(t);
        (self.set_left_bar)(t);
        if let Some(s) = self.active.borrow().as_ref() {
            (s.apply_tool)(t);
        }
    }

    fn next_untitled_title(&self) -> String {
        let n = self.untitled_counter.get() + 1;
        self.untitled_counter.set(n);
        if n == 1 {
            "Untitled".to_string()
        } else {
            format!("Untitled {n}")
        }
    }

    /// Create a blank document of the given size and open it in a new tab.
    pub(crate) fn new_document(self: &Rc<Self>, size: Size) -> Rc<DocumentSession> {
        let title = self.next_untitled_title();
        let session = DocumentSession::new(
            &self.global,
            &self.set_active_tool_late,
            size,
            self.history_capacity,
            title,
        );
        self.add_session(&session);
        session
    }

    /// Register an already-built session as a tab and select it.
    pub(crate) fn add_session(self: &Rc<Self>, session: &Rc<DocumentSession>) {
        let page = self.tab_view.add_page(&session.canvas_root, None);
        page.set_title(&session.display_title());
        *session.tab_page.borrow_mut() = Some(page.clone());
        self.sessions.borrow_mut().push(Rc::clone(session));
        // Selecting fires `selected-page`, which calls `activate`; also activate
        // directly so the first tab is wired before any signal plumbing exists.
        self.tab_view.set_selected_page(&page);
        self.activate(session);
    }

    /// Make `session` the active document: swap its chrome into the window slots,
    /// re-point the doc-scoped gio actions, and sync the shared tool visuals.
    pub(crate) fn activate(&self, session: &Rc<DocumentSession>) {
        // Close any liquify session on the tab being left. The active tool is
        // global but `apply_tool` only reaches the foreground document, so a
        // session on a background tab would otherwise stay live: its preview
        // splice hides later edits to that layer (`Canvas::present` branches on
        // renderer state, not on the tool), and the next bake would overwrite
        // them from a stale snapshot with no history entry.
        if let Some(previous) = self.active.borrow().as_ref()
            && !Rc::ptr_eq(previous, session)
        {
            (previous.liquify_flush)();
        }
        *self.active.borrow_mut() = Some(Rc::clone(session));
        set_slot_child(&self.tool_options_slot, &session.tool_options);
        set_slot_child(&self.right_bar_slot, &session.right_bar);
        (session.reinstall_actions)();
        // Transient toasts are drawn into the active document's canvas surface;
        // re-point them so a toast lands on the tab the user is looking at.
        self.global
            .toaster
            .set_target(session.viewport.paintable().clone());

        // Reflect the shared tool in this document's panels without the
        // destructive side effects of a full tool switch (no transform lift,
        // no crop default rect). Those only run on an explicit tool change.
        let t = self.global.tools.active.get();
        (session.set_tool_options)(t);
        (session.set_right_panel_tool)(t);
        session.viewport.paintable().set_crop_active(t == Tool::Crop);
        session.viewport.paintable().set_transform_active(t == Tool::Transform);
        session.viewport.redraw_handle().request();
    }

    fn session_for_page(&self, page: &adw::TabPage) -> Option<Rc<DocumentSession>> {
        self.sessions
            .borrow()
            .iter()
            .find(|s| s.tab_page.borrow().as_ref() == Some(page))
            .cloned()
    }

    /// `selected-page` handler: activate the document behind the new page.
    pub(crate) fn on_page_selected(&self) {
        if let Some(page) = self.tab_view.selected_page()
            && let Some(session) = self.session_for_page(&page)
        {
            self.activate(&session);
        }
    }

    /// `close-page` handler. Returns whether we are handling the close
    /// asynchronously (`Stop`) - i.e. an unsaved document needs confirmation.
    pub(crate) fn on_close_page(self: &Rc<Self>, page: &adw::TabPage) -> glib::Propagation {
        let Some(session) = self.session_for_page(page) else {
            return glib::Propagation::Proceed;
        };
        if !session.is_dirty() {
            return glib::Propagation::Proceed;
        }
        self.confirm_close(page, &session);
        glib::Propagation::Stop
    }

    fn confirm_close(self: &Rc<Self>, page: &adw::TabPage, session: &Rc<DocumentSession>) {
        let dialog = gtk::AlertDialog::builder()
            .message("Save changes before closing?")
            .detail(format!(
                "\"{}\" has unsaved changes that will be lost.",
                session.title.borrow()
            ))
            .modal(true)
            .build();
        dialog.set_buttons(&["Cancel", "Discard", "Save"]);
        dialog.set_cancel_button(0);
        dialog.set_default_button(2);

        let manager = Rc::clone(self);
        let page = page.clone();
        let session = Rc::clone(session);
        dialog.choose(
            Some(&self.root),
            None::<&gio::Cancellable>,
            move |result| match result {
                Ok(2) => {
                    // Save: write straight to disk when a path exists, then
                    // close. Without a path, keep the tab open and prompt for a
                    // location (the user can close again once it's saved).
                    if session.file_path.borrow().is_some() {
                        crate::project_io::save(&session, &manager.root, false);
                        manager.tab_view.close_page_finish(&page, true);
                    } else {
                        manager.tab_view.close_page_finish(&page, false);
                        crate::project_io::save(&session, &manager.root, true);
                    }
                }
                Ok(1) => manager.tab_view.close_page_finish(&page, true),
                _ => manager.tab_view.close_page_finish(&page, false),
            },
        );
    }

    /// Window close-request handler: if any open document has unsaved changes,
    /// hold the window open and route every tab through the normal close flow so
    /// each dirty document gets a Save/Discard/Cancel prompt. The window closes
    /// once the last tab is gone (via `on_page_detached`).
    pub(crate) fn on_window_close_request(self: &Rc<Self>) -> glib::Propagation {
        let any_dirty = self.sessions.borrow().iter().any(|s| s.is_dirty());
        if !any_dirty {
            return glib::Propagation::Proceed;
        }
        let pages: Vec<adw::TabPage> =
            (0..self.tab_view.n_pages()).map(|i| self.tab_view.nth_page(i)).collect();
        for page in &pages {
            self.tab_view.close_page(page);
        }
        glib::Propagation::Stop
    }

    /// `page-detached` handler: drop the session and close the window if the
    /// last tab is gone.
    pub(crate) fn on_page_detached(self: &Rc<Self>, page: &adw::TabPage) {
        if let Some(session) = self.session_for_page(page) {
            // A closed tab no longer needs its autosave recovery copy.
            session.clear_recovery();
        }
        self.sessions
            .borrow_mut()
            .retain(|s| s.tab_page.borrow().as_ref() != Some(page));
        if self.sessions.borrow().is_empty() {
            self.root.close();
        }
    }

    /// Tick that autosaves the open documents once the configured interval has
    /// elapsed. Reads enabled/interval live, so preferences changes apply at
    /// once.
    pub(crate) fn start_autosave_timer(self: &Rc<Self>) {
        // The finest interval offered is 10s, so a 5s tick is granular enough.
        const TICK: Duration = Duration::from_secs(5);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(TICK, move || {
            let Some(manager) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let cfg = &manager.global.autosave;
            if !cfg.enabled.get() {
                // Keep the clock from firing a burst the moment autosave is re-enabled.
                manager.last_autosave.set(Instant::now());
                return glib::ControlFlow::Continue;
            }
            let interval = u64::from(cfg.interval_secs.get().max(1));
            if manager.last_autosave.get().elapsed().as_secs() < interval {
                return glib::ControlFlow::Continue;
            }
            manager.last_autosave.set(Instant::now());
            let sessions = manager.sessions.borrow().clone();
            crate::project_io::autosave_all(sessions, manager.root.clone());
            glib::ControlFlow::Continue
        });
    }

    /// Open a loaded project in a fresh tab.
    pub(crate) fn open_loaded(self: &Rc<Self>, project: OxieProject, path: PathBuf) {
        let size = Size::new(project.document.canvas_width, project.document.canvas_height);
        let title = path.file_stem().map_or_else(
            || "Untitled".to_string(),
            |s| s.to_string_lossy().into_owned(),
        );
        let session = DocumentSession::new(
            &self.global,
            &self.set_active_tool_late,
            size,
            self.history_capacity,
            title,
        );

        // Load embedded fonts into the shared engine so text layers render and
        // stay editable even if those fonts aren't installed on this machine.
        if !project.font_bytes.is_empty() {
            let mut engine = self.global.text_engine.borrow_mut();
            for bytes in project.font_bytes.values() {
                engine.load_font_data(bytes.clone());
            }
        }

        {
            let canvas = session.viewport.canvas();
            if let Err(e) = project::load::apply(&project, &mut canvas.borrow_mut()) {
                crate::project_io::show_error(&self.root, "Open Failed", &e.to_string());
                return;
            }
        }
        // Restore the document's default gradient stops (if the file has any).
        session
            .gradient
            .settings
            .borrow_mut()
            .clone_from(&project.document.gradient);

        // Restore the persisted view rotation. The centering tick that runs on
        // the first frame re-centres pan for this angle (see fit_and_center).
        session
            .viewport
            .set_rotation_raw(project.document.view_rotation);

        // Restore the persisted drawing guide and push its symmetry + overlay
        // to the canvas (notify_changed drives the session's guide sync).
        session
            .guide
            .config
            .borrow_mut()
            .clone_from(&project.document.guide);
        session.guide.notify_changed();

        // Restore the per-document component library.
        *session.components.borrow_mut() = project::load::build_components(&project);
        (session.refresh_components)();
        *session.file_path.borrow_mut() = Some(path);
        session.mark_saved();
        (session.refresh_layers)();
        session.viewport.resync_canvas_size();
        session.viewport.redraw_handle().request();

        self.add_session(&session);
    }

    /// Wire the tab signals (selection / close / detach) to this manager.
    pub(crate) fn connect_tab_signals(self: &Rc<Self>) {
        {
            let manager = Rc::clone(self);
            self.tab_view
                .connect_selected_page_notify(move |_| manager.on_page_selected());
        }
        {
            let manager = Rc::clone(self);
            self.tab_view
                .connect_close_page(move |_, page| manager.on_close_page(page));
        }
        {
            let manager = Rc::clone(self);
            self.tab_view
                .connect_page_detached(move |_, page, _| manager.on_page_detached(page));
        }
    }

    /// Register the File-menu gio actions (New / Open / Save / Save As / Close /
    /// Quit) plus the per-document edit/select/zoom/tool actions, all routed to
    /// the active tab.
    pub(crate) fn register_actions(self: &Rc<Self>, app: &gtk::Application) {
        // -- New --
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("new", None);
            action.connect_activate(move |_, _| {
                manager.new_document(Size::new(2048, 2048));
            });
            app.add_action(&action);
        }
        // -- Open --
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("open", None);
            action.connect_activate(move |_, _| {
                let m = Rc::clone(&manager);
                let on_loaded: Rc<dyn Fn(OxieProject, PathBuf)> =
                    Rc::new(move |project, path| m.open_loaded(project, path));
                crate::project_io::open_dialog(&manager.root, on_loaded);
            });
            app.add_action(&action);
        }
        // -- Save / Save As --
        for (id, force_dialog) in [("save", false), ("save-as", true)] {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new(id, None);
            action.connect_activate(move |_, _| {
                if let Some(session) = manager.active() {
                    crate::project_io::save(&session, &manager.root, force_dialog);
                }
            });
            app.add_action(&action);
        }
        // -- Close Tab --
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("close-tab", None);
            action.connect_activate(move |_, _| {
                if let Some(page) = manager.tab_view.selected_page() {
                    manager.tab_view.close_page(&page);
                }
            });
            app.add_action(&action);
        }
        // -- Quit --
        {
            let app_c = app.clone();
            let action = gio::SimpleAction::new("quit", None);
            action.connect_activate(move |_, _| app_c.quit());
            app.add_action(&action);
        }

        // -- Undo / Redo --
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("undo", None);
            action.connect_activate(move |_, _| {
                if let Some(s) = manager.active() {
                    s.undo();
                }
            });
            app.add_action(&action);
        }
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("redo", None);
            action.connect_activate(move |_, _| {
                if let Some(s) = manager.active() {
                    s.redo();
                }
            });
            app.add_action(&action);
        }
        // -- Rename (active layer/group, or selected component) --
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("rename", None);
            action.connect_activate(move |_, _| {
                if let Some(s) = manager.active() {
                    (s.begin_rename)();
                }
            });
            app.add_action(&action);
        }

        // -- Selection --
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("select-all", None);
            action.connect_activate(move |_, _| {
                if let Some(s) = manager.active() {
                    s.select_all();
                }
            });
            app.add_action(&action);
        }
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("deselect-all", None);
            action.connect_activate(move |_, _| {
                if let Some(s) = manager.active() {
                    s.deselect();
                }
            });
            app.add_action(&action);
        }
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("select-inverse", None);
            action.connect_activate(move |_, _| {
                if let Some(s) = manager.active() {
                    s.select_inverse();
                }
            });
            app.add_action(&action);
        }

        // -- Tool select --
        let tool_actions: &[(&str, Tool)] = &[
            ("select-cursor", Tool::Cursor),
            (
                "select-selection",
                Tool::Selection(oxiedraw_core::tools::SelectionTool::Square),
            ),
            ("select-transform", Tool::Transform),
            ("select-brush", Tool::Brush),
            ("select-picker", Tool::ColorPicker),
            (
                "select-fill",
                Tool::Fill(oxiedraw_core::tools::FillTool::Bucket),
            ),
            ("select-text", Tool::Text),
            ("select-crop", Tool::Crop),
            ("select-liquify", Tool::Liquify),
            ("select-guide", Tool::DrawingGuide),
        ];
        for &(id, tool) in tool_actions {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new(id, None);
            action.connect_activate(move |_, _| manager.set_active_tool(tool));
            app.add_action(&action);
        }

        // Drawing Guide commit / cancel (top-bar buttons). Done keeps the live
        // config; Cancel restores the snapshot taken when the tool was entered.
        {
            let manager = Rc::clone(self);
            let done = gio::SimpleAction::new("guide-done", None);
            done.connect_activate(move |_, _| manager.set_active_tool(Tool::Brush));
            app.add_action(&done);
        }
        {
            let manager = Rc::clone(self);
            let cancel = gio::SimpleAction::new("guide-cancel", None);
            cancel.connect_activate(move |_, _| {
                if let Some(s) = manager.active.borrow().as_ref() {
                    let snapshot = s.guide.entry_snapshot.borrow().clone();
                    *s.guide.config.borrow_mut() = snapshot;
                    s.guide.notify_changed();
                }
                manager.set_active_tool(Tool::Brush);
            });
            app.add_action(&cancel);
        }

        // Liquify Apply / Cancel / Restore All (top-bar buttons, mirroring the
        // Crop tool). Apply and Cancel both leave the tool; Restore All only
        // zeroes the field, so the user stays in Liquify and can keep warping.
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("liquify-apply", None);
            // Each stroke is already baked and recorded, so Apply is just
            // "I'm done" - the tool switch closes the session.
            action.connect_activate(move |_, _| manager.set_active_tool(Tool::Brush));
            app.add_action(&action);
        }
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("liquify-cancel", None);
            action.connect_activate(move |_, _| {
                if let Some(s) = manager.active() {
                    (s.liquify_cancel)();
                }
                manager.set_active_tool(Tool::Brush);
            });
            app.add_action(&action);
        }
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("liquify-restore", None);
            action.connect_activate(move |_, _| {
                if let Some(s) = manager.active() {
                    (s.liquify_restore)();
                }
            });
            app.add_action(&action);
        }

        // Eraser mode toggle (brush). Stateful boolean action: the brush bar's
        // toggle button binds to it by name (so every tab's button reflects the
        // shared state), and the keybinding activates it. The handler mirrors
        // the state into the shared ToolState that the stroke path reads.
        {
            let manager = Rc::clone(self);
            let action =
                gio::SimpleAction::new_stateful("eraser-toggle", None, &false.to_variant());
            action.connect_change_state(move |action, state| {
                let on = state.and_then(glib::Variant::get::<bool>).unwrap_or(false);
                manager.global.tools.eraser.set(on);
                action.set_state(&on.to_variant());
            });
            app.add_action(&action);
        }
    }
}

/// Replace the single child of a slot container with `child`. The previous
/// child is unparented (it is owned by its document session, not the slot).
fn set_slot_child(slot: &gtk::Box, child: &gtk::Widget) {
    while let Some(existing) = slot.first_child() {
        slot.remove(&existing);
    }
    slot.append(child);
}
