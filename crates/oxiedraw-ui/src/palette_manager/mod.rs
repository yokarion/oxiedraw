//! Manage Palettes window.
//!
//! Two-pane libadwaita window opened via `app.palette-manager`, laid out like
//! Manage Brushes: every palette on the left under its own header, the
//! selected one's editor on the right with the preview pinned below it. The
//! shipped palettes are read-only; everything else edits the live
//! [`PaletteState`], which saves itself a moment later.

pub(crate) mod extract;
mod shared;

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::color::{Color, ColorSlot, ColorState};
use oxiedraw_core::palettes::{PaletteState, reorder};
use relm4::gtk;
use relm4::gtk::gio;

use self::shared::PalettePreview;
use crate::toaster::Toaster;
use crate::widgets::color_strip::ColorStrip;
use crate::widgets::swatch_grid::{GridContent, GridHooks, SwatchGrid};

const WINDOW_WIDTH: i32 = 980;
const WINDOW_HEIGHT: i32 = 680;
const SIDEBAR_WIDTH: i32 = 340;
const ROW_STRIP: (i32, i32) = (110, 20);
/// Tall enough that the wheel keeps its RGB and hex inputs, which it drops
/// below a height meant for a cramped dock.
const PICKER_HEIGHT: i32 = 390;

/// The palette the right pane is editing. Tracked by name because that is what
/// survives a rebuild of the list.
type Selection = Rc<RefCell<Option<String>>>;

#[derive(Clone)]
struct Manager {
    palettes: PaletteState,
    colors: ColorState,
    toaster: Toaster,
    selected: Selection,
    grid: Rc<SwatchGrid>,
    preview: Rc<PalettePreview>,
    name_row: adw::EntryRow,
    read_only: adw::PreferencesGroup,
    count_label: gtk::Label,
    active_label: gtk::Label,
    show_button: gtk::Button,
    add_popover: gtk::Popover,
    listbox: gtk::ListBox,
    rows: Rc<RefCell<Vec<(String, gtk::ListBoxRow)>>>,
    syncing: Rc<Cell<bool>>,
}

impl Manager {
    fn selected_name(&self) -> Option<String> {
        self.selected.borrow().clone()
    }

    /// Move the editor to another palette. Not a full refresh: rebuilding the
    /// list destroys the row being clicked or arrowed onto, focus with it.
    fn select(&self, name: Option<String>) {
        *self.selected.borrow_mut() = name;
        self.sync_selected_row();
        self.refresh_editor();
    }

    fn sync_selected_row(&self) {
        let wanted = self.selected_name();
        let row = wanted.and_then(|name| {
            self.rows
                .borrow()
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, row)| row.clone())
        });
        self.syncing.set(true);
        match row {
            Some(row) => self.listbox.select_row(Some(&row)),
            None => self.listbox.unselect_all(),
        }
        self.syncing.set(false);
    }

    /// Both panes, from the store. Called for every change, which keeps the
    /// list counts and the strips honest after an edit on the right.
    fn refresh(&self) {
        self.rebuild_list();
        self.refresh_editor();
    }

    fn rebuild_list(&self) {
        self.syncing.set(true);
        while let Some(child) = self.listbox.first_child() {
            self.listbox.remove(&child);
        }
        self.rows.borrow_mut().clear();

        let (names, selected) = {
            let store = self.palettes.store();
            (store.listed_names(), self.selected_name())
        };
        let mut row_to_select = None;
        for name in names {
            let (colors, favorite) = {
                let store = self.palettes.store();
                (
                    store.palette(&name).map(|p| p.colors.clone()).unwrap_or_default(),
                    store.is_favorite(&name),
                )
            };
            let row = self.build_row(&name, &colors, favorite);
            self.listbox.append(&row);
            if selected.as_deref() == Some(name.as_str()) {
                row_to_select = Some(row.clone());
            }
            self.rows.borrow_mut().push((name, row));
        }
        if let Some(row) = row_to_select {
            self.listbox.select_row(Some(&row));
        }
        self.syncing.set(false);
    }

    fn build_row(&self, name: &str, colors: &[Color], favorite: bool) -> gtk::ListBoxRow {
        // Same spacing as the Manage Brushes rows, so the stars line up the
        // same way in both windows.
        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .build();

        let star = crate::brush_picker::shared::star_button(favorite, "Favorite");
        {
            let manager = self.clone();
            let name = name.to_string();
            star.connect_clicked(move |_| {
                let on = !manager.palettes.store().is_favorite(&name);
                manager.palettes.edit(|store| store.set_favorite(&name, on));
            });
        }
        row_box.append(&star);

        row_box.append(
            &gtk::Label::builder()
                .label(name)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        );
        row_box.append(
            &gtk::Label::builder()
                .label(colors.len().to_string())
                .css_classes(["dim-label", "caption"])
                .build(),
        );

        let strip = ColorStrip::sized(ROW_STRIP.0, ROW_STRIP.1);
        strip.set_colors(colors);
        row_box.append(&strip.widget());

        gtk::ListBoxRow::builder().child(&row_box).build()
    }

    fn refresh_editor(&self) {
        let Some(name) = self.selected_name() else {
            self.clear_editor();
            return;
        };
        let store = self.palettes.store();
        let Some(palette) = store.palette(&name) else {
            drop(store);
            self.clear_editor();
            return;
        };
        let (colors, builtin) = (palette.colors.clone(), palette.builtin);
        let is_active = store.active_preset == name;
        drop(store);

        self.syncing.set(true);
        self.set_editor_sensitive(true);
        if self.name_row.text() != name {
            self.name_row.set_text(&name);
        }
        self.name_row.set_editable(!builtin);
        self.read_only.set_visible(builtin);
        self.count_label.set_text(&colors.len().to_string());
        self.active_label.set_visible(is_active);
        self.show_button.set_visible(!is_active);
        self.preview.set_colors(&colors);
        self.grid.set_content(GridContent {
            colors,
            capacity: 0,
            add_slot: !builtin,
            editable: !builtin,
        });
        self.grid.set_selected(self.grid.index_of(self.colors.current()));
        self.syncing.set(false);
    }

    /// Nothing selected: empty the right pane rather than leaving the last
    /// palette's colours sitting there looking editable.
    fn clear_editor(&self) {
        self.syncing.set(true);
        self.set_editor_sensitive(false);
        self.name_row.set_text("");
        self.read_only.set_visible(false);
        self.count_label.set_text("0");
        self.active_label.set_visible(false);
        self.show_button.set_visible(false);
        self.preview.set_colors(&[]);
        self.grid.set_content(GridContent::default());
        self.syncing.set(false);
    }

    fn set_editor_sensitive(&self, on: bool) {
        for widget in [
            self.name_row.clone().upcast::<gtk::Widget>(),
            self.grid.widget(),
            self.show_button.clone().upcast(),
        ] {
            widget.set_sensitive(on);
        }
    }

    /// Run `change` against the selected palette's colours.
    fn edit_colors(&self, change: impl FnOnce(&mut Vec<Color>) -> bool) {
        let Some(name) = self.selected_name() else {
            return;
        };
        self.palettes.edit(|store| {
            store.palette_mut(&name).is_some_and(|palette| {
                !palette.builtin && change(&mut palette.colors)
            })
        });
    }

    fn commit_rename(&self) {
        let Some(from) = self.selected_name() else {
            return;
        };
        let to = self.name_row.text().trim().to_string();
        if to == from {
            return;
        }
        // Set before the edit: it notifies synchronously, and the rebuilt list
        // looks the selection up by name.
        *self.selected.borrow_mut() = Some(to.clone());
        if !self.palettes.edit(|store| store.rename_palette(&from, &to)) {
            *self.selected.borrow_mut() = Some(from.clone());
            self.toaster.error(&format!("Couldn't rename to \"{to}\""));
            self.name_row.set_text(&from);
        }
    }

    fn duplicate(&self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let copy = self.palettes.edit(|store| store.duplicate_palette(&name));
        if let Some(copy) = copy {
            self.select(Some(copy));
            self.name_row.grab_focus();
        }
    }

    fn delete(&self, parent: &gtk::Window) {
        let Some(name) = self.selected_name() else {
            return;
        };
        if self.palettes.store().palette(&name).is_some_and(|p| p.builtin) {
            self.toaster
                .error("Built-in palettes can't be deleted - duplicate one instead");
            return;
        }
        let dialog = gtk::AlertDialog::builder()
            .message("Delete Palette")
            .detail(format!("Permanently remove \"{name}\"? This cannot be undone."))
            .modal(true)
            .build();
        dialog.set_buttons(&["Cancel", "Delete"]);
        dialog.set_default_button(0);
        dialog.set_cancel_button(0);

        let manager = self.clone();
        dialog.choose(Some(parent), None::<&gio::Cancellable>, move |result| {
            if result != Ok(1) {
                return;
            }
            if manager.palettes.edit(|store| store.remove_palette(&name)) {
                manager.select(None);
                manager.toaster.info(&format!("Deleted \"{name}\""));
            }
        });
    }

    fn add_palette(&self) {
        let name = self.palettes.edit(|store| store.add_palette("New Palette", Vec::new()));
        self.select(Some(name));
        self.name_row.grab_focus();
    }

    /// Open the wheel beside the "+" cell, starting from the colour being
    /// painted with. It stays open, so several colours go in one after another.
    fn open_add_picker(&self, cell: &gtk::gdk::Rectangle) {
        let (picker_colors, picker) = add_picker();
        picker_colors.selected.set(ColorSlot::Primary);
        picker_colors.primary.set(self.colors.current());
        picker_colors.notify_changed();

        if let Some(content) = self.add_popover.child().and_downcast::<gtk::Box>()
            && picker.parent().as_ref() != Some(content.upcast_ref())
        {
            detach_picker(picker);
            content.prepend(picker);
        }
        self.add_popover.set_pointing_to(Some(cell));
        self.add_popover.popup();
    }

}

// Deliberately not a `Manager` method: the popover's button holding a manager
// would hold the popover itself, a cycle nothing ever tears down.
fn add_picked_color(palettes: &PaletteState, selected: &Selection, toaster: &Toaster) {
    let Some(name) = selected.borrow().clone() else {
        return;
    };
    let color = add_picker().0.current();
    let added = palettes.edit(|store| {
        store.palette_mut(&name).is_some_and(|palette| {
            let fresh = !palette.builtin && !palette.colors.contains(&color);
            if fresh {
                palette.colors.push(color);
            }
            fresh
        })
    });
    if !added {
        toaster.info(&format!("{} is already in this palette", color.to_hex()));
    }
}

// One wheel shared by every Manage Palettes window, and never freed: the
// picker's change hook keeps its own state alive, so building one per window
// would leak one per window instead.
thread_local! {
    static ADD_PICKER: OnceCell<&'static (ColorState, gtk::Widget)> = const { OnceCell::new() };
}

fn add_picker() -> &'static (ColorState, gtk::Widget) {
    ADD_PICKER.with(|cell| {
        *cell.get_or_init(|| {
            let colors = ColorState::new();
            let picker = crate::panels::color_picker::build(colors.clone());
            picker.remove_css_class("oxiedraw-chrome");
            picker.set_height_request(PICKER_HEIGHT);
            Box::leak(Box::new((colors, picker.upcast())))
        })
    })
}

/// The wheel only if some window has already opened it - closing a window that
/// never did should not build one.
fn built_add_picker() -> Option<&'static (ColorState, gtk::Widget)> {
    ADD_PICKER.with(|cell| cell.get().copied())
}

fn detach_picker(picker: &gtk::Widget) {
    if let Some(parent) = picker.parent().and_downcast::<gtk::Box>() {
        parent.remove(picker);
    }
}

pub(crate) fn show(
    parent: &adw::ApplicationWindow,
    palettes: &PaletteState,
    colors: &ColorState,
    toaster: &Toaster,
) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(WINDOW_WIDTH)
        .default_height(WINDOW_HEIGHT)
        .title("Manage Palettes")
        .build();

    let manager = Manager {
        palettes: palettes.clone(),
        colors: colors.clone(),
        toaster: toaster.clone(),
        selected: Rc::new(RefCell::new(
            palettes.store().active_palette().map(|p| p.name.clone()),
        )),
        grid: Rc::new(SwatchGrid::new()),
        preview: Rc::new(PalettePreview::new()),
        name_row: adw::EntryRow::builder().title("Name").build(),
        read_only: build_read_only_group(),
        count_label: gtk::Label::builder()
            .css_classes(["dim-label"])
            .valign(gtk::Align::Center)
            .build(),
        active_label: gtk::Label::builder()
            .label("Active")
            .css_classes(["heading"])
            .valign(gtk::Align::Center)
            .build(),
        show_button: gtk::Button::builder()
            .label("Show")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build(),
        add_popover: gtk::Popover::new(),
        listbox: gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build(),
        rows: Rc::new(RefCell::new(Vec::new())),
        syncing: Rc::new(Cell::new(false)),
    };

    install_editor_hooks(&manager);
    build_add_popover(&manager);

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&build_sidebar(&window, &manager))
        .end_child(&build_editor(&manager))
        .resize_start_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(SIDEBAR_WIDTH)
        .build();
    window.set_content(Some(&paned));

    let observer = {
        let manager = manager.clone();
        palettes.connect_changed(Box::new(move || manager.refresh()))
    };
    let color_observer = {
        // Weak grid: the ring only needs updating while the window is up.
        let grid = Rc::downgrade(&manager.grid);
        let current = colors.clone();
        colors.connect_changed(Box::new(move || {
            if let Some(grid) = grid.upgrade() {
                grid.set_selected(grid.index_of(current.current()));
            }
        }))
    };
    {
        let palettes = palettes.clone();
        let colors = colors.clone();
        let manager = manager.clone();
        window.connect_close_request(move |_| {
            palettes.disconnect_changed(observer);
            colors.disconnect_changed(color_observer);
            // The grid's hooks hold the manager that holds the grid; nothing
            // else would ever break that loop.
            manager.grid.set_hooks(GridHooks::default());
            // Hand the shared wheel back before the popover goes, and take
            // the popover off the grid it was parented to.
            if let Some((_, picker)) = built_add_picker() {
                detach_picker(picker);
            }
            manager.add_popover.unparent();
            gtk::glib::Propagation::Proceed
        });
    }

    manager.refresh();
    window.present();
}

fn build_sidebar(window: &adw::Window, manager: &Manager) -> gtk::Widget {
    let title = gtk::Label::builder()
        .label("Manage Palettes")
        .css_classes(["heading"])
        .build();
    let header = adw::HeaderBar::builder()
        .title_widget(&title)
        .show_end_title_buttons(false)
        .build();

    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .has_frame(false)
        .tooltip_text("New palette")
        .build();
    {
        let manager = manager.clone();
        add.connect_clicked(move |_| manager.add_palette());
    }
    header.pack_end(&add);

    let menu = gio::Menu::new();
    menu.append(Some("Duplicate palette"), Some("palettes.duplicate"));
    menu.append(Some("Delete palette"), Some("palettes.delete"));
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .has_frame(false)
            .menu_model(&menu)
            .build(),
    );
    install_window_actions(window, manager);

    {
        let manager = manager.clone();
        manager.listbox.clone().connect_row_selected(move |_, row| {
            if manager.syncing.get() {
                return;
            }
            let Some(row) = row else { return };
            let name = manager
                .rows
                .borrow()
                .iter()
                .find(|(_, candidate)| candidate == row)
                .map(|(name, _)| name.clone());
            if name != manager.selected_name() {
                manager.select(name);
            }
        });
    }

    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["oxiedraw-chrome"])
        .build();
    outer.append(&header);
    outer.append(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&manager.listbox)
            .build(),
    );
    outer.upcast()
}

fn install_window_actions(window: &adw::Window, manager: &Manager) {
    let group = gio::SimpleActionGroup::new();
    let duplicate = gio::SimpleAction::new("duplicate", None);
    {
        let manager = manager.clone();
        duplicate.connect_activate(move |_, _| manager.duplicate());
    }
    group.add_action(&duplicate);
    let delete = gio::SimpleAction::new("delete", None);
    {
        // Weak, or the action group would hold the window that owns it.
        let manager = manager.clone();
        let window = window.downgrade();
        delete.connect_activate(move |_, _| {
            if let Some(window) = window.upgrade() {
                manager.delete(window.upcast_ref());
            }
        });
    }
    group.add_action(&delete);
    window.insert_action_group("palettes", Some(&group));
}

fn build_editor(manager: &Manager) -> gtk::Widget {
    // Empty title: this bar only carries the close button, and keeps the
    // right pane's top edge level with the sidebar's header.
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .title_widget(&gtk::Label::new(None))
        .build();

    let body = shared::body();

    let identity = adw::PreferencesGroup::new();
    identity.add(&manager.name_row);
    body.append(&identity);
    body.append(&manager.read_only);
    body.append(&shared::colors_group(&manager.grid, None));

    let properties = adw::PreferencesGroup::builder().title("Properties").build();
    let count_row = adw::ActionRow::builder().title("Colors").build();
    count_row.add_suffix(&manager.count_label);
    properties.add(&count_row);
    let shown_row = adw::ActionRow::builder()
        .title("Shown on the Presets tab")
        .build();
    shown_row.add_suffix(&manager.active_label);
    shown_row.add_suffix(&manager.show_button);
    properties.add(&shown_row);
    body.append(&properties);

    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    outer.append(&header);
    outer.append(&shared::scroller(&body));
    outer.append(&manager.preview.widget());
    outer.upcast()
}

fn build_add_popover(manager: &Manager) {
    let add = gtk::Button::builder()
        .label("Add Color")
        .css_classes(["suggested-action"])
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(12)
        .build();
    {
        let palettes = manager.palettes.clone();
        let selected = Rc::clone(&manager.selected);
        let toaster = manager.toaster.clone();
        add.connect_clicked(move |_| add_picked_color(&palettes, &selected, &toaster));
    }
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content.append(&add);
    manager.add_popover.set_child(Some(&content));
    manager.add_popover.set_parent(&manager.grid.widget());
}

fn install_editor_hooks(manager: &Manager) {
    manager.grid.set_hooks(GridHooks {
        pick: Some({
            let manager = manager.clone();
            Rc::new(move |index| {
                let color = manager.selected_name().and_then(|name| {
                    manager
                        .palettes
                        .store()
                        .palette(&name)
                        .and_then(|p| p.colors.get(index).copied())
                });
                if let Some(color) = color {
                    manager.colors.set_current(color);
                    manager.colors.notify_changed();
                }
            })
        }),
        add: Some({
            let manager = manager.clone();
            Rc::new(move |cell| manager.open_add_picker(&cell))
        }),
        reorder: Some({
            let manager = manager.clone();
            Rc::new(move |from, to| {
                manager.edit_colors(|colors| reorder(colors, from, to));
            })
        }),
        remove: Some({
            let manager = manager.clone();
            Rc::new(move |index| {
                manager.edit_colors(|colors| {
                    if index >= colors.len() {
                        return false;
                    }
                    colors.remove(index);
                    true
                });
            })
        }),
    });

    {
        let manager = manager.clone();
        manager
            .name_row
            .clone()
            .connect_entry_activated(move |_| manager.commit_rename());
    }
    {
        let row = manager.name_row.clone();
        let manager = manager.clone();
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| {
            if !manager.syncing.get() {
                manager.commit_rename();
            }
        });
        row.add_controller(focus);
    }
    {
        let manager = manager.clone();
        manager.show_button.clone().connect_clicked(move |_| {
            let Some(name) = manager.selected_name() else {
                return;
            };
            manager.palettes.edit(|store| {
                if store.active_preset == name {
                    return false;
                }
                store.active_preset = name;
                true
            });
        });
    }
}

fn build_read_only_group() -> adw::PreferencesGroup {
    let row = adw::ActionRow::builder()
        .title("Built-in palette - duplicate it to make changes.")
        .build();
    row.add_suffix(
        &gtk::Label::builder()
            .label("Read-only")
            .css_classes(["dim-label", "caption"])
            .valign(gtk::Align::Center)
            .build(),
    );
    let group = adw::PreferencesGroup::new();
    group.add(&row);
    group
}

