
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use relm4::gtk::glib;

use crate::layout::{
    FloatAnchor, FloatSize, Layout, LayoutSettings, PanelId, SplitChild, ToolWindowId,
};
use crate::settings::AppSettings;

use super::DockHost;

pub(crate) struct LayoutManager {
    settings: RefCell<LayoutSettings>,
    dock: Rc<DockHost>,
    edit_mode: Cell<bool>,
    reanchor: RefCell<Option<Rc<dyn Fn(&Layout)>>>,
    on_changed: RefCell<Vec<Rc<dyn Fn()>>>,
    on_edit_mode: RefCell<Vec<Rc<dyn Fn(bool)>>>,
    save_queued: Cell<bool>,
}

impl LayoutManager {
    pub(crate) fn new(dock: &Rc<DockHost>, mut settings: LayoutSettings) -> Rc<Self> {
        settings.sanitize();
        Rc::new(Self {
            settings: RefCell::new(settings),
            dock: Rc::clone(dock),
            edit_mode: Cell::new(false),
            reanchor: RefCell::new(None),
            on_changed: RefCell::new(Vec::new()),
            on_edit_mode: RefCell::new(Vec::new()),
            save_queued: Cell::new(false),
        })
    }

    pub(crate) fn set_reanchor(&self, reanchor: Rc<dyn Fn(&Layout)>) {
        *self.reanchor.borrow_mut() = Some(reanchor);
    }

    pub(crate) fn connect_changed(&self, observer: Rc<dyn Fn()>) {
        self.on_changed.borrow_mut().push(observer);
    }

    pub(crate) fn connect_edit_mode(&self, observer: Rc<dyn Fn(bool)>) {
        self.on_edit_mode.borrow_mut().push(observer);
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.settings
            .borrow()
            .layouts
            .iter()
            .map(|l| l.name.clone())
            .collect()
    }

    pub(crate) fn current_name(&self) -> String {
        self.settings.borrow().current.clone()
    }

    pub(crate) fn current_index(&self) -> usize {
        self.settings.borrow().active_index()
    }

    pub(crate) fn current_is_preset(&self) -> bool {
        self.settings.borrow().active().preset.is_some()
    }

    pub(crate) fn current_layout(&self) -> Layout {
        self.settings.borrow().active().clone()
    }

    pub(crate) fn is_panel_visible(&self, id: PanelId) -> bool {
        self.settings.borrow().active().is_panel_visible(id)
    }

    pub(crate) fn can_delete(&self) -> bool {
        self.settings.borrow().layouts.len() > 1
    }

    pub(crate) fn edit_mode(&self) -> bool {
        self.edit_mode.get()
    }

    pub(crate) fn set_edit_mode(&self, on: bool) {
        if self.edit_mode.replace(on) == on {
            return;
        }
        let observers = self.on_edit_mode.borrow().clone();
        for observer in observers {
            observer(on);
        }
    }

    pub(crate) fn select(&self, name: &str) {
        {
            let mut settings = self.settings.borrow_mut();
            if settings.current == name || !settings.layouts.iter().any(|l| l.name == name) {
                return;
            }
            settings.current = name.to_string();
        }
        tracing::info!(target: "oxiedraw::layout", layout = %name, "layout selected");
        self.apply();
        self.notify_changed();
    }

    pub(crate) fn duplicate(&self) -> String {
        let name = self.settings.borrow_mut().duplicate_active();
        self.save();
        self.notify_changed();
        name
    }

    pub(crate) fn rename_current(&self, to: &str) -> bool {
        let from = self.current_name();
        let renamed = self.settings.borrow_mut().rename(&from, to);
        if renamed {
            self.save();
            self.notify_changed();
        }
        renamed
    }

    pub(crate) fn remove(&self, name: &str) {
        if !self.settings.borrow_mut().remove(name) {
            return;
        }
        tracing::info!(target: "oxiedraw::layout", layout = %name, "layout deleted");
        self.apply();
        self.notify_changed();
    }

    pub(crate) fn reset(&self) {
        if !self.settings.borrow_mut().active_mut().reset_to_preset() {
            return;
        }
        self.apply();
        self.notify_changed();
    }

    pub(crate) fn set_panel_visible(&self, id: PanelId, visible: bool) {
        self.edit(|layout| layout.set_panel_visible(id, visible));
    }

    pub(crate) fn set_float_anchor(&self, window: ToolWindowId, anchor: FloatAnchor) {
        self.settings
            .borrow_mut()
            .active_mut()
            .set_anchor(window, anchor);
        let layout = self.settings.borrow().active().clone();
        let reanchor = self.reanchor.borrow().clone();
        if let Some(reanchor) = reanchor {
            reanchor(&layout);
        }
        self.save();
    }

    pub(crate) fn set_float_size(&self, window: ToolWindowId, size: FloatSize) {
        self.settings
            .borrow_mut()
            .active_mut()
            .set_float_size(window, size);
        let layout = self.settings.borrow().active().clone();
        let reanchor = self.reanchor.borrow().clone();
        if let Some(reanchor) = reanchor {
            reanchor(&layout);
        }
        self.save();
    }

    pub(crate) fn edit(&self, change: impl FnOnce(&mut Layout)) {
        {
            let mut settings = self.settings.borrow_mut();
            change(settings.active_mut());
        }
        self.apply();
        // Left stale, the box for a panel just switched off swallows the click
        // that would bring it back.
        self.notify_changed();
    }

    // Deliberately does not rebuild: the widgets are already this size, and
    // tearing the tree down mid-drag would take the handle with it.
    pub(crate) fn store_split_size(self: &Rc<Self>, path: &[SplitChild], px: i32) -> bool {
        let changed = self
            .settings
            .borrow_mut()
            .active_mut()
            .root
            .set_split_size(path, px);
        if changed {
            self.save_soon();
        }
        changed
    }

    pub(crate) fn flush(&self) {
        if self.save_queued.replace(false) {
            self.save();
        }
    }

    fn save_soon(self: &Rc<Self>) {
        if self.save_queued.replace(true) {
            return;
        }
        let manager = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            if let Some(manager) = manager.upgrade() {
                manager.save_queued.set(false);
                manager.save();
            }
        });
    }

    pub(crate) fn apply(&self) {
        let layout = self.settings.borrow().active().clone();
        self.dock.rebuild(&layout);
        let reanchor = self.reanchor.borrow().clone();
        if let Some(reanchor) = reanchor {
            reanchor(&layout);
        }
        self.save();
    }

    pub(crate) fn save(&self) {
        let mut settings = AppSettings::load();
        settings.layout = self.settings.borrow().clone();
        settings.save();
    }

    fn notify_changed(&self) {
        let observers = self.on_changed.borrow().clone();
        for observer in observers {
            observer();
        }
    }
}
