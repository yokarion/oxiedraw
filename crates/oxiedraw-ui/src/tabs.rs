// Owns the open documents, puts the active one's panels in the dock's slots,
// and routes the File-menu operations to whichever tab is in front.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::gio;
use oxiedraw_core::project::{self, format::OxieProject};
use oxiedraw_core::tools::Tool;
use oxiedraw_utils::frame_profile;
use oxiedraw_utils::geometry::Size;
use relm4::gtk;
use relm4::gtk::glib;

use crate::canvas::Viewport;
use crate::layout::PanelId;
use crate::session::{DocumentSession, GlobalState, SetActiveToolSlot};

pub(crate) struct TabManager {
    pub(crate) global: GlobalState,
    pub(crate) set_active_tool_late: SetActiveToolSlot,
    pub(crate) set_left_bar: Rc<dyn Fn(Tool)>,
    pub(crate) tab_view: adw::TabView,
    pub(crate) dock: Rc<crate::dock::DockHost>,
    pub(crate) layouts: Rc<crate::dock::manager::LayoutManager>,
    pub(crate) sessions: RefCell<Vec<Rc<DocumentSession>>>,
    pub(crate) active: RefCell<Option<Rc<DocumentSession>>>,
    pub(crate) root: adw::ApplicationWindow,
    pub(crate) history_capacity: usize,
    pub(crate) untitled_counter: Cell<u32>,
    pub(crate) last_autosave: Cell<Instant>,
}

impl TabManager {
    pub(crate) fn active(&self) -> Option<Rc<DocumentSession>> {
        self.active.borrow().clone()
    }

    pub(crate) fn active_viewport_provider(self: &Rc<Self>) -> Rc<dyn Fn() -> Option<Viewport>> {
        let manager = Rc::clone(self);
        Rc::new(move || manager.active().map(|s| s.viewport.clone()))
    }

    pub(crate) fn set_active_tool(&self, t: Tool) {
        let previous = self.global.tools.active.get();
        if previous != t {
            tracing::info!(
                target: "oxiedraw::tool",
                tool = t.display_name(),
                from = previous.display_name(),
                "tool selected"
            );
        }
        if let Some(s) = self.active.borrow().as_ref() {
            s.text_edit.commit();
            if t != Tool::Pattern {
                s.pattern_edit.leave();
            }
        }
        self.global.tools.active.set(t);
        (self.set_left_bar)(t);
        if let Some(s) = self.active.borrow().as_ref() {
            (s.apply_tool)(t);
        }
    }

    pub(crate) fn set_guide_enabled(&self, on: bool) {
        let Some(session) = self.active() else { return };
        if on {
            if let Some(stashed) = session.guide.stash.borrow_mut().take() {
                *session.guide.config.borrow_mut() = Some(stashed);
            }
            self.set_active_tool(Tool::DrawingGuide);
        } else {
            let live = session.guide.config.borrow_mut().take();
            *session.guide.stash.borrow_mut() = live;
            session.guide.notify_changed();
            if self.global.tools.active.get() == Tool::DrawingGuide {
                self.set_active_tool(Tool::Brush);
            }
        }
        let kind = session
            .guide
            .config
            .borrow()
            .as_ref()
            .or(session.guide.stash.borrow().as_ref())
            .map(|c| c.kind);
        tracing::info!(target: "oxiedraw::tool", on, ?kind, "drawing guide toggled");
    }

    pub(crate) fn sync_guide_toggle(&self) {
        let on = self
            .active()
            .is_some_and(|s| s.guide.config.borrow().is_some());
        if let Some(gio_app) = gio::Application::default()
            && let Some(action) = gio_app.lookup_action("guide-toggle")
            && let Ok(action) = action.downcast::<gio::SimpleAction>()
        {
            action.set_state(&on.to_variant());
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

    pub(crate) fn new_document(self: &Rc<Self>, size: Size) -> Rc<DocumentSession> {
        let title = self.next_untitled_title();
        tracing::info!(
            target: "oxiedraw::doc",
            title = %title,
            width = size.width,
            height = size.height,
            "document created"
        );
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

    pub(crate) fn add_session(self: &Rc<Self>, session: &Rc<DocumentSession>) {
        session
            .tool_windows
            .apply_anchors(&self.layouts.current_layout());

        {
            let manager = Rc::downgrade(self);
            let owner = Rc::downgrade(session);
            session.guide.connect_changed(Box::new(move || {
                let (Some(manager), Some(owner)) = (manager.upgrade(), owner.upgrade()) else {
                    return;
                };
                if manager.active().is_some_and(|a| Rc::ptr_eq(&a, &owner)) {
                    manager.sync_guide_toggle();
                }
            }));
        }

        let page = self.tab_view.add_page(&session.canvas_root, None);
        page.set_title(&session.display_title());
        *session.tab_page.borrow_mut() = Some(page.clone());
        self.sessions.borrow_mut().push(Rc::clone(session));
        self.tab_view.set_selected_page(&page);
        self.activate(session);
    }

    pub(crate) fn activate(&self, session: &Rc<DocumentSession>) {
        // `apply_tool` only reaches the foreground document, so a session left
        // live on a background tab later bakes over it from a stale snapshot.
        if let Some(previous) = self.active.borrow().as_ref()
            && !Rc::ptr_eq(previous, session)
        {
            (previous.liquify_flush)();
            previous.pattern_edit.leave();
        }
        let switched = self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|p| !Rc::ptr_eq(p, session));
        if switched {
            tracing::info!(
                target: "oxiedraw::doc",
                title = %session.display_title(),
                open_tabs = self.sessions.borrow().len(),
                "tab switched"
            );
        }
        *self.active.borrow_mut() = Some(Rc::clone(session));
        self.fill_document_panels();
        (session.reinstall_actions)();
        self.global
            .toaster
            .set_target(session.viewport.paintable().clone());

        let t = self.global.tools.active.get();
        (session.set_tool_options)(t);
        (session.set_tool_window)(t);
        self.sync_guide_toggle();
        session.viewport.paintable().set_crop_active(t == Tool::Crop);
        session.viewport.paintable().set_transform_active(t == Tool::Transform);
        session.viewport.paintable().set_guide_editing(t == Tool::DrawingGuide);
        session.viewport.redraw_handle().request();
    }

    pub(crate) fn fill_document_panels(&self) {
        let Some(session) = self.active() else { return };
        for (id, widget) in [
            (PanelId::ToolOptions, &session.tool_options),
            (PanelId::ColorPicker, &session.color_picker),
            (PanelId::Layers, &session.layers),
            (PanelId::CanvasInfo, &session.canvas_info),
        ] {
            self.dock.fill(id, widget);
        }
    }

    fn session_for_page(&self, page: &adw::TabPage) -> Option<Rc<DocumentSession>> {
        self.sessions
            .borrow()
            .iter()
            .find(|s| s.tab_page.borrow().as_ref() == Some(page))
            .cloned()
    }

    pub(crate) fn on_page_selected(&self) {
        if let Some(page) = self.tab_view.selected_page()
            && let Some(session) = self.session_for_page(&page)
        {
            self.activate(&session);
        }
    }

    pub(crate) fn on_close_page(self: &Rc<Self>, page: &adw::TabPage) -> glib::Propagation {
        let Some(session) = self.session_for_page(page) else {
            return glib::Propagation::Proceed;
        };
        session.pattern_edit.leave();
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

    pub(crate) fn on_window_close_request(self: &Rc<Self>) -> glib::Propagation {
        self.layouts.flush();
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

    pub(crate) fn on_page_detached(self: &Rc<Self>, page: &adw::TabPage) {
        if let Some(session) = self.session_for_page(page) {
            session.clear_recovery();
        }
        self.sessions
            .borrow_mut()
            .retain(|s| s.tab_page.borrow().as_ref() != Some(page));
        if self.sessions.borrow().is_empty() {
            self.root.close();
        }
    }

    pub(crate) fn start_autosave_timer(self: &Rc<Self>) {
        const TICK: Duration = Duration::from_secs(5);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(TICK, move || {
            let _span = frame_profile::span(frame_profile::Stage::Timers);
            let Some(manager) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let cfg = &manager.global.autosave;
            if !cfg.enabled.get() {
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
        session
            .gradient
            .settings
            .borrow_mut()
            .clone_from(&project.document.gradient);

        session
            .viewport
            .set_rotation_raw(project.document.view_rotation);

        session
            .guide
            .config
            .borrow_mut()
            .clone_from(&project.document.guide);
        session.guide.notify_changed();

        *session.components.borrow_mut() = project::load::build_components(&project);
        (session.refresh_components)();
        *session.file_path.borrow_mut() = Some(path);
        session.mark_saved();
        (session.refresh_layers)();
        session.viewport.resync_canvas_size();
        session.viewport.redraw_handle().request();

        self.add_session(&session);
    }

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

    pub(crate) fn register_actions(self: &Rc<Self>, app: &gtk::Application) {
        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("new", None);
            action.connect_activate(move |_, _| {
                manager.new_document(Size::new(2048, 2048));
            });
            app.add_action(&action);
        }
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
        {
            let app_c = app.clone();
            let action = gio::SimpleAction::new("quit", None);
            action.connect_activate(move |_, _| app_c.quit());
            app.add_action(&action);
        }

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
            ("select-pattern", Tool::Pattern),
        ];
        for &(id, tool) in tool_actions {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new(id, None);
            action.connect_activate(move |_, _| manager.set_active_tool(tool));
            app.add_action(&action);
        }

        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new_stateful("guide-toggle", None, &false.to_variant());
            action.connect_change_state(move |_, state| {
                let on = state.and_then(glib::Variant::get::<bool>).unwrap_or(false);
                manager.set_guide_enabled(on);
                manager.sync_guide_toggle();
            });
            app.add_action(&action);
        }

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
                    if snapshot.is_some() {
                        *s.guide.config.borrow_mut() = snapshot;
                        s.guide.notify_changed();
                    }
                }
                manager.set_active_tool(Tool::Brush);
            });
            app.add_action(&cancel);
        }

        {
            let manager = Rc::clone(self);
            let action = gio::SimpleAction::new("liquify-apply", None);
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

