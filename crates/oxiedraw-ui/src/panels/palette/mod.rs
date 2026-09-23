//! The palette dock: Recent, Custom and Presets over one shared swatch grid.
//!
//! Recent holds the colours actually drawn with (see
//! [`ColorState::notify_used`]), newest first. Custom is the user's own list
//! and the only one they can drag around. Presets shows one palette at a time,
//! built-in or their own. All three read and write the app-wide
//! [`PaletteState`]; the `palette.*` action group below is what the header menu
//! drives, and the two windows it can open are `app.` actions because they need
//! the open document.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_core::color::{Color, ColorState};
use oxiedraw_core::enum_meta::EnumMeta;
use oxiedraw_core::palettes::{PaletteState, SortKey, reorder, sort_colors};
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::gio;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::toaster::Toaster;
use crate::widgets::swatch_grid::{GridContent, GridHooks, SwatchGrid};

const MARGIN: i32 = 8;

/// Colours previewed beside each name in the presets dropdown, as slim bars.
const PREVIEW_CHIPS: usize = 12;
const CHIP_WIDTH: f64 = 6.0;
const CHIP_HEIGHT: f64 = 16.0;
const CHIP_GAP: f64 = 2.0;
const CHIP_RADIUS: f64 = 2.0;

/// Name label and check of every popup row the dropdown has built, so the
/// check can follow the shown preset rather than the row under the pointer.
type PresetChecks = Rc<RefCell<Vec<(glib::WeakRef<gtk::Label>, glib::WeakRef<gtk::Image>)>>>;

/// Header, the preset row and one row of swatches. Anything shorter clips the
/// grid away entirely.
pub(crate) const MIN_SIZE: i32 = 112;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Recent,
    Custom,
    Presets,
}

impl EnumMeta for Tab {
    const ALL: &'static [Self] = &[Self::Recent, Self::Custom, Self::Presets];

    fn label(self) -> &'static str {
        match self {
            Self::Recent => "Recent",
            Self::Custom => "Custom",
            Self::Presets => "Presets",
        }
    }
}

#[derive(Clone)]
struct Panel {
    colors: ColorState,
    palettes: PaletteState,
    toaster: Toaster,
    tab: Rc<Cell<Tab>>,
    grid: Rc<SwatchGrid>,
    preset_row: gtk::Box,
    presets: gtk::DropDown,
    preset_names: gtk::StringList,
    /// Names plus previewed colours, as last listed: rows rebind only when the
    /// model changes, so a recoloured palette has to force that itself.
    preset_listing: Rc<RefCell<Vec<(String, Vec<Color>)>>>,
    preset_checks: PresetChecks,
    copy_button: gtk::Button,
    tab_buttons: Rc<RefCell<Vec<(Tab, gtk::ToggleButton)>>>,
    sort_actions: Rc<Vec<gio::SimpleAction>>,
    save_custom_action: gio::SimpleAction,
    /// Last preset the dock showed, so a palette chosen in the extractor or the
    /// manage window can bring the Presets tab forward.
    last_preset: Rc<RefCell<String>>,
    syncing: Rc<Cell<bool>>,
}

impl Panel {
    /// The colours the current tab shows, and whether the user may rearrange
    /// them (Recent is chronological, and a built-in preset is read-only).
    fn visible_colors(&self) -> (Vec<Color>, bool) {
        let store = self.palettes.store();
        match self.tab.get() {
            Tab::Recent => (store.recent.clone(), false),
            Tab::Custom => (store.custom.clone(), true),
            Tab::Presets => store.active_palette().map_or_else(
                || (Vec::new(), false),
                |palette| (palette.colors.clone(), !palette.builtin),
            ),
        }
    }

    fn pick(&self, index: usize) {
        let Some(color) = self.visible_colors().0.get(index).copied() else {
            return;
        };
        self.colors.set_current(color);
        self.colors.notify_changed();
    }

    fn add_active_color(&self) {
        let color = self.colors.current();
        if self.palettes.edit(|store| store.add_custom(color)) {
            self.toaster.info(&format!("Added {} to Custom", color.to_hex()));
        } else {
            self.toaster.info(&format!("{} is already in Custom", color.to_hex()));
        }
    }

    /// Sort the list on show, if it is theirs to rearrange - the menu entries
    /// are insensitive otherwise.
    fn sort(&self, key: SortKey) {
        let tab = self.tab.get();
        self.palettes.edit(|store| match tab {
            Tab::Custom => {
                sort_colors(&mut store.custom, key);
                true
            }
            Tab::Presets => {
                let name = store.active_preset.clone();
                store.palette_mut(&name).is_some_and(|palette| {
                    if palette.builtin {
                        return false;
                    }
                    sort_colors(&mut palette.colors, key);
                    true
                })
            }
            Tab::Recent => false,
        });
    }

    /// Turn the Custom list into a preset of its own, named like the layout
    /// bar's duplicate does: created first, renamed afterwards.
    fn save_custom_as_palette(&self) {
        let colors = self.palettes.store().custom.clone();
        if colors.is_empty() {
            self.toaster.info("Custom is empty - save some swatches first");
            return;
        }
        let count = colors.len();
        let name = self.palettes.edit(|store| {
            let name = store.add_palette("Custom Palette", colors);
            store.active_preset.clone_from(&name);
            name
        });
        self.toaster
            .info(&format!("Saved \"{name}\" with {count} colors"));
    }

    fn copy_preset_to_custom(&self) {
        let colors = self.visible_colors().0;
        let added = self.palettes.edit(|store| {
            colors
                .iter()
                .filter(|color| store.add_custom(**color))
                .count()
        });
        self.toaster.info(&match added {
            0 => "Every color is already in Custom".to_string(),
            1 => "Copied 1 color to Custom".to_string(),
            n => format!("Copied {n} colors to Custom"),
        });
        self.set_tab(Tab::Custom);
    }

    fn move_swatch(&self, from: usize, to: usize) {
        let tab = self.tab.get();
        self.palettes.edit(|store| match tab {
            Tab::Custom => reorder(&mut store.custom, from, to),
            Tab::Presets => {
                let name = store.active_preset.clone();
                store.palette_mut(&name).is_some_and(|palette| {
                    !palette.builtin && reorder(&mut palette.colors, from, to)
                })
            }
            Tab::Recent => false,
        });
    }

    fn remove_swatch(&self, index: usize) {
        let tab = self.tab.get();
        let removed = self.palettes.edit(|store| match tab {
            Tab::Custom => {
                (index < store.custom.len()).then(|| store.custom.remove(index))
            }
            Tab::Presets => {
                let name = store.active_preset.clone();
                store.palette_mut(&name).and_then(|palette| {
                    (!palette.builtin && index < palette.colors.len())
                        .then(|| palette.colors.remove(index))
                })
            }
            Tab::Recent => None,
        });
        if let Some(color) = removed {
            self.toaster.info(&format!("Removed {}", color.to_hex()));
        }
    }

    /// Rebuild the header, dropdown and grid from the store - the one entry
    /// point, so every change lands the same way.
    fn refresh(&self) {
        // A palette chosen elsewhere - the extractor, or Show in the manage
        // window - brings its tab forward, since the user just asked to see it.
        let active_preset = self.palettes.store().active_preset.clone();
        if self.last_preset.replace(active_preset.clone()) != active_preset
            && self.tab.get() != Tab::Presets
        {
            self.set_tab(Tab::Presets);
            return;
        }

        let tab = self.tab.get();
        let (colors, editable) = self.visible_colors();

        self.preset_row.set_visible(tab == Tab::Presets);
        self.copy_button.set_sensitive(!colors.is_empty());
        for action in self.sort_actions.iter() {
            action.set_enabled(editable && colors.len() > 1);
        }
        self.save_custom_action
            .set_enabled(!self.palettes.store().custom.is_empty());

        if tab == Tab::Presets {
            self.sync_preset_dropdown();
        }

        self.grid.set_content(GridContent {
            colors,
            capacity: 0,
            add_slot: tab == Tab::Custom,
            editable,
        });
        self.sync_selection();
    }

    fn sync_preset_dropdown(&self) {
        let (listing, selected) = {
            let store = self.palettes.store();
            let listing: Vec<(String, Vec<Color>)> = store
                .listed_names()
                .into_iter()
                .map(|name| {
                    let preview = store
                        .palette(&name)
                        .map(|p| p.colors.iter().take(PREVIEW_CHIPS).copied().collect())
                        .unwrap_or_default();
                    (name, preview)
                })
                .collect();
            let selected = listing
                .iter()
                .position(|(name, _)| *name == store.active_preset)
                .unwrap_or(0);
            (listing, selected)
        };

        self.syncing.set(true);
        if *self.preset_listing.borrow() != listing {
            let names: Vec<&str> = listing.iter().map(|(name, _)| name.as_str()).collect();
            self.preset_names
                .splice(0, self.preset_names.n_items(), &names);
            *self.preset_listing.borrow_mut() = listing;
        }
        self.presets.set_selected(selected as u32);
        self.syncing.set(false);
        self.sync_preset_checks();
    }

    fn sync_preset_checks(&self) {
        let active = self.palettes.store().active_preset.clone();
        self.preset_checks.borrow_mut().retain(|(label, check)| {
            let (Some(label), Some(check)) = (label.upgrade(), check.upgrade()) else {
                return false;
            };
            check.set_visible(label.text() == active);
            true
        });
    }

    /// Switch tabs as if the user had clicked one: the button drives the
    /// change, so its state and the panel's never disagree.
    fn set_tab(&self, tab: Tab) {
        let button = self
            .tab_buttons
            .borrow()
            .iter()
            .find(|(candidate, _)| *candidate == tab)
            .map(|(_, button)| button.clone());
        if let Some(button) = button {
            button.set_active(true);
        } else {
            // Before the header exists, e.g. the refresh during build.
            self.tab.set(tab);
            self.refresh();
        }
    }

    /// Ring the swatch holding the active colour, if the visible list has it.
    fn sync_selection(&self) {
        self.grid.set_selected(self.grid.index_of(self.colors.current()));
    }
}

pub(crate) fn build(
    colors: &ColorState,
    palettes: &PaletteState,
    toaster: &Toaster,
) -> gtk::Box {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .hexpand(true)
        .vexpand(true)
        .css_classes(["oxiedraw-chrome"])
        .build();
    root.set_margin_top(MARGIN);
    root.set_margin_bottom(MARGIN);
    root.set_margin_start(MARGIN);
    root.set_margin_end(MARGIN);

    let sort_actions: Vec<gio::SimpleAction> = SortKey::ALL
        .iter()
        .map(|key| gio::SimpleAction::new(&sort_action_name(*key), None))
        .collect();

    let grid = Rc::new(SwatchGrid::new());

    let preset_names = gtk::StringList::new(&[]);
    let preset_checks: PresetChecks = Rc::new(RefCell::new(Vec::new()));
    let presets = gtk::DropDown::builder()
        .model(&preset_names)
        .factory(&preset_button_factory())
        .list_factory(&preset_list_factory(palettes, &preset_checks))
        .hexpand(true)
        .valign(gtk::Align::Center)
        .tooltip_text("Palette shown on the Presets tab")
        .build();
    crate::top_bar::style_control(&presets);

    let copy_button = gtk::Button::builder()
        .icon_name("oxiedraw-brush-arrow-down-symbolic")
        .has_frame(false)
        .valign(gtk::Align::Center)
        .tooltip_text("Copy this palette into Custom")
        .build();
    crate::top_bar::style_control(&copy_button);

    let panel = Panel {
        colors: colors.clone(),
        palettes: palettes.clone(),
        toaster: toaster.clone(),
        tab: Rc::new(Cell::new(Tab::Recent)),
        grid: Rc::clone(&grid),
        preset_row: gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .build(),
        presets,
        preset_names,
        preset_listing: Rc::new(RefCell::new(Vec::new())),
        preset_checks,
        copy_button,
        tab_buttons: Rc::new(RefCell::new(Vec::new())),
        sort_actions: Rc::new(sort_actions),
        save_custom_action: gio::SimpleAction::new("save-custom", None),
        last_preset: Rc::new(RefCell::new(palettes.store().active_preset.clone())),
        syncing: Rc::new(Cell::new(false)),
    };

    let refresh: Rc<dyn Fn()> = {
        let panel = panel.clone();
        Rc::new(move || panel.refresh())
    };

    install_grid_hooks(&grid, &panel);
    root.append(&build_header(&panel, &refresh));
    root.append(&build_preset_row(&panel, &refresh));

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .vexpand(true)
        .child(&grid.widget())
        .build();
    root.append(&scroller);

    root.insert_action_group("palette", Some(&build_actions(&panel)));

    {
        // A pick reorders Recent under the grid, so every change rebuilds the
        // tab rather than trying to move the ring on its own.
        let refresh = Rc::clone(&refresh);
        palettes.connect_changed(Box::new(move || refresh()));
    }
    {
        let panel = panel.clone();
        colors.connect_changed(Box::new(move || panel.sync_selection()));
    }

    refresh();
    root
}

fn install_grid_hooks(grid: &Rc<SwatchGrid>, panel: &Panel) {
    let pick: Rc<dyn Fn(usize)> = {
        let panel = panel.clone();
        Rc::new(move |index| panel.pick(index))
    };
    let add: Rc<dyn Fn(gtk::gdk::Rectangle)> = {
        let panel = panel.clone();
        Rc::new(move |_| panel.add_active_color())
    };
    let reorder: Rc<dyn Fn(usize, usize)> = {
        let panel = panel.clone();
        Rc::new(move |from, to| panel.move_swatch(from, to))
    };
    let remove: Rc<dyn Fn(usize)> = {
        let panel = panel.clone();
        Rc::new(move |index| panel.remove_swatch(index))
    };
    grid.set_hooks(GridHooks {
        pick: Some(pick),
        add: Some(add),
        reorder: Some(reorder),
        remove: Some(remove),
    });
}

fn build_header(panel: &Panel, refresh: &Rc<dyn Fn()>) -> gtk::Box {
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();

    // Same control as the layout switcher in the top bar, so the two read as
    // one family.
    let tabs = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .valign(gtk::Align::Center)
        .css_classes(["linked"])
        .build();
    crate::top_bar::style_control(&tabs);
    let mut group: Option<gtk::ToggleButton> = None;
    for tab in Tab::ALL {
        // Never ellipsized: the labels' full width is the dock's floor, so it
        // stops shrinking before a tab name would be cut.
        let button = gtk::ToggleButton::builder().label(tab.label()).build();
        // Grouped before the active one is set: joining a group clears it.
        if let Some(first) = &group {
            button.set_group(Some(first));
        } else {
            group = Some(button.clone());
        }
        button.set_active(*tab == panel.tab.get());
        {
            let panel = panel.clone();
            let refresh = Rc::clone(refresh);
            let tab = *tab;
            button.connect_toggled(move |button| {
                if button.is_active() && panel.tab.replace(tab) != tab {
                    panel.grid.cancel_drag();
                    refresh();
                }
            });
        }
        panel.tab_buttons.borrow_mut().push((*tab, button.clone()));
        tabs.append(&button);
    }
    header.append(&tabs);

    header.append(&gtk::Box::builder().hexpand(true).build());

    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .has_frame(false)
        .valign(gtk::Align::Center)
        .tooltip_text("Add active color to Custom")
        .action_name("palette.add-active")
        .build();
    crate::top_bar::style_control(&add);
    header.append(&add);

    let menu = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .has_frame(false)
        .valign(gtk::Align::Center)
        .tooltip_text("Palette options")
        .menu_model(&build_menu())
        .build();
    crate::top_bar::style_control(&menu);
    header.append(&menu);

    header
}

fn build_preset_row(panel: &Panel, refresh: &Rc<dyn Fn()>) -> gtk::Box {
    {
        let panel = panel.clone();
        let refresh = Rc::clone(refresh);
        panel.presets.connect_selected_notify(move |dropdown| {
            if panel.syncing.get() {
                return;
            }
            let Some(name) = panel
                .preset_names
                .string(dropdown.selected())
                .map(|s| s.to_string())
            else {
                return;
            };
            panel.palettes.edit(|store| {
                if store.active_preset == name {
                    return false;
                }
                store.active_preset = name;
                true
            });
            refresh();
        });
    }
    {
        let copy_button = panel.copy_button.clone();
        let panel = panel.clone();
        copy_button.connect_clicked(move |_| panel.copy_preset_to_custom());
    }

    panel.preset_row.append(&panel.presets);
    panel.preset_row.append(&panel.copy_button);
    panel.preset_row.clone()
}

fn item_name(item: &gtk::ListItem) -> Option<String> {
    item.item()
        .and_downcast::<gtk::StringObject>()
        .map(|s| s.string().to_string())
}

// The closed button shows the name alone, ellipsized: a long palette name
// then shrinks the button instead of widening the dock, which is what pushed
// the canvas around when the Presets tab opened.
fn preset_button_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            let label = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            item.set_child(Some(&label));
        }
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        if let (Some(label), Some(name)) = (item.child().and_downcast::<gtk::Label>(), item_name(item)) {
            label.set_text(&name);
        }
    });
    factory
}

// Popup rows: name, colours as slim bars, and a check on the preset shown. It
// follows `active_preset`, not the row's own selection, which tracks the hover.
fn preset_list_factory(palettes: &PaletteState, checks: &PresetChecks) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    {
        let checks = Rc::clone(checks);
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(10)
                .build();
            let label = gtk::Label::builder().xalign(0.0).hexpand(true).build();
            row.append(&label);
            let chips_width = PREVIEW_CHIPS as f64 * (CHIP_WIDTH + CHIP_GAP) - CHIP_GAP;
            row.append(
                &gtk::DrawingArea::builder()
                    .content_width(chips_width.ceil() as i32)
                    .content_height(CHIP_HEIGHT as i32)
                    .valign(gtk::Align::Center)
                    .build(),
            );
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.set_visible(false);
            let check_slot = gtk::Box::builder().width_request(16).build();
            check_slot.append(&check);
            row.append(&check_slot);
            checks.borrow_mut().push((label.downgrade(), check.downgrade()));
            item.set_child(Some(&row));
        });
    }

    let palettes = palettes.clone();
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(name), Some(row)) = (item_name(item), item.child()) else {
            return;
        };
        let label = row.first_child().and_downcast::<gtk::Label>();
        let chips = label
            .as_ref()
            .and_then(WidgetExt::next_sibling)
            .and_downcast::<gtk::DrawingArea>();
        let check = row
            .last_child()
            .and_then(|slot| slot.first_child())
            .and_downcast::<gtk::Image>();
        if let Some(label) = label {
            label.set_text(&name);
        }
        let (colors, active): (Vec<Color>, bool) = {
            let store = palettes.store();
            (
                store
                    .palette(&name)
                    .map(|p| p.colors.iter().take(PREVIEW_CHIPS).copied().collect())
                    .unwrap_or_default(),
                store.active_preset == name,
            )
        };
        if let Some(check) = check {
            check.set_visible(active);
        }
        if let Some(chips) = chips {
            chips.set_draw_func(move |_, cr, _, _| paint_chips(cr, &colors));
            chips.queue_draw();
        }
    });
    factory
}

fn paint_chips(cr: &cairo::Context, colors: &[Color]) {
    for (index, color) in colors.iter().enumerate() {
        let x = index as f64 * (CHIP_WIDTH + CHIP_GAP);
        crate::widgets::shapes::rounded_rect(
            cr,
            x,
            0.0,
            CHIP_WIDTH,
            CHIP_HEIGHT,
            CHIP_RADIUS,
        );
        cr.set_source_rgb(
            f64::from(color.r) / 255.0,
            f64::from(color.g) / 255.0,
            f64::from(color.b) / 255.0,
        );
        cr.fill_preserve().ok();
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.35);
        cr.set_line_width(1.0);
        cr.stroke().ok();
    }
}

fn build_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let colors = gio::Menu::new();
    colors.append(Some("Save Custom as palette"), Some("palette.save-custom"));
    colors.append(
        Some("Extract palette from canvas..."),
        Some("palette.extract"),
    );
    menu.append_section(None, &colors);

    let sorting = gio::Menu::new();
    for key in SortKey::ALL {
        sorting.append(
            Some(&format!("Sort by {}", key.label())),
            Some(&format!("palette.{}", sort_action_name(*key))),
        );
    }
    menu.append_section(None, &sorting);

    let manage = gio::Menu::new();
    manage.append(Some("Manage palettes..."), Some("palette.manage"));
    menu.append_section(None, &manage);

    menu
}

fn sort_action_name(key: SortKey) -> String {
    format!("sort-{}", key.label().to_lowercase())
}

fn build_actions(panel: &Panel) -> gio::SimpleActionGroup {
    let group = gio::SimpleActionGroup::new();

    let add = gio::SimpleAction::new("add-active", None);
    {
        let panel = panel.clone();
        add.connect_activate(move |_, _| panel.add_active_color());
    }
    group.add_action(&add);

    {
        let panel = panel.clone();
        panel
            .save_custom_action
            .clone()
            .connect_activate(move |_, _| panel.save_custom_as_palette());
    }
    group.add_action(&panel.save_custom_action);

    for (action, key) in panel.sort_actions.iter().zip(SortKey::ALL) {
        let panel = panel.clone();
        let key = *key;
        action.connect_activate(move |_, _| panel.sort(key));
        group.add_action(action);
    }

    // Both windows need the foreground document, which only the app-level
    // actions can reach; the panel just asks for them.
    for (local, app_action) in [
        ("extract", "palette-extract"),
        ("manage", "palette-manager"),
    ] {
        let action = gio::SimpleAction::new(local, None);
        action.connect_activate(move |_, _| {
            if let Some(app) = gio::Application::default() {
                app.activate_action(app_action, None);
            }
        });
        group.add_action(&action);
    }

    group
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_actions_are_named_after_their_key() {
        assert_eq!(sort_action_name(SortKey::Hue), "sort-hue");
        assert_eq!(sort_action_name(SortKey::Brightness), "sort-brightness");
        assert_eq!(sort_action_name(SortKey::Saturation), "sort-saturation");
    }

    #[test]
    fn every_tab_has_a_label() {
        assert_eq!(Tab::ALL.len(), 3);
        assert_eq!(Tab::from_index(1), Tab::Custom);
        assert_eq!(Tab::Presets.to_index(), 2);
    }
}
