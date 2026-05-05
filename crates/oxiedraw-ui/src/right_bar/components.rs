//! Components tab in the right sidebar.
//!
//! Shows the per-document component library as a grid of flat dark preview
//! cards, each with its name centred below it. Empty components show a
//! placeholder glyph. Cards can be selected, removed (X, top-right), and
//! opened for editing (double-click). A "New" button appends a blank one.

use std::cell::RefCell;
use std::rc::Rc;

use oxiedraw_core::components::ComponentLibrary;
use oxiedraw_core::history::{HistoryAction, HistoryStack};
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::gdk;
use relm4::gtk::gio;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

const CARD_HEIGHT: i32 = 104;
const CARD_BG: (f64, f64, f64) = (0.20, 0.20, 0.21);
const CHECKER_SIZE: f64 = 8.0;
const GRID_SPACING: u32 = 8;
const FALLBACK_ACCENT: (f64, f64, f64) = (0.21, 0.52, 0.89);

/// Late-bound grid-rebuild closure. Cards capture the slot (not the closure
/// itself) to avoid an Rc cycle, and read it back when they need a rebuild.
type RefreshSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// Build the Components page. Returns the page widget and a `refresh` closure
/// that rebuilds the card grid from the current library state (call after the
/// library changes outside this panel, e.g. on leaving edit mode).
pub(super) fn build(
    library: Rc<RefCell<ComponentLibrary>>,
    on_edit: Rc<dyn Fn(String)>,
    history: Rc<RefCell<HistoryStack>>,
) -> (gtk::Box, Rc<dyn Fn()>, Rc<dyn Fn()>) {
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .vexpand(true)
        .hexpand(true)
        .build();

    // Currently selected component id (visual highlight only).
    let selected: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let refresh_slot: RefreshSlot = Rc::new(RefCell::new(None));
    // Card DrawingAreas of the current grid, so a selection change can redraw
    // them in place instead of rebuilding the grid.
    let card_areas: Rc<RefCell<Vec<gtk::DrawingArea>>> = Rc::new(RefCell::new(Vec::new()));

    // A plain Grid rather than a FlowBox: FlowBox grabs button presses for its
    // own selection/activation, which swallows the cards' click + drag-source
    // gestures. Grid is a pure layout container and passes events through.
    let grid = gtk::Grid::builder()
        .column_spacing(GRID_SPACING as i32)
        .row_spacing(GRID_SPACING as i32)
        .column_homogeneous(true)
        .valign(gtk::Align::Start)
        .build();

    let empty_label = gtk::Label::builder()
        .label("No components yet.\nClick New to create one.")
        .justify(gtk::Justification::Center)
        .wrap(true)
        .build();
    empty_label.add_css_class("dim-label");
    empty_label.set_vexpand(true);

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::None)
        .vexpand(true)
        .hexpand(true)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .child(&grid)
        .build();
    stack.add_named(&scroll, Some("grid"));
    stack.add_named(&empty_label, Some("empty"));

    // -- refresh: rebuild the grid from the library --------------------
    let refresh: Rc<dyn Fn()> = {
        let grid = grid.clone();
        let stack = stack.clone();
        let library = Rc::clone(&library);
        let selected = Rc::clone(&selected);
        let on_edit = Rc::clone(&on_edit);
        let refresh_slot = Rc::clone(&refresh_slot);
        let card_areas = Rc::clone(&card_areas);
        let history = Rc::clone(&history);
        Rc::new(move || {
            while let Some(child) = grid.first_child() {
                grid.remove(&child);
            }
            card_areas.borrow_mut().clear();
            let lib = library.borrow();
            if lib.is_empty() {
                stack.set_visible_child_name("empty");
                return;
            }
            stack.set_visible_child_name("grid");
            for (i, component) in lib.components.iter().enumerate() {
                let card = build_card(
                    &component.id,
                    &component.name,
                    &component.master,
                    component.size.width,
                    component.size.height,
                    &library,
                    &selected,
                    &on_edit,
                    &refresh_slot,
                    &card_areas,
                    &history,
                );
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let (col, row) = ((i % 2) as i32, (i / 2) as i32);
                grid.attach(&card, col, row, 1, 1);
            }
        })
    };
    *refresh_slot.borrow_mut() = Some(Rc::clone(&refresh));

    // -- header (New button) -------------------------------------------
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    let new_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .label("Create new component")
        .hexpand(true)
        .halign(gtk::Align::Fill)
        .build();
    {
        let library = Rc::clone(&library);
        let selected = Rc::clone(&selected);
        let refresh = Rc::clone(&refresh);
        let history = Rc::clone(&history);
        new_btn.connect_clicked(move |_| {
            let name = next_component_name(&library.borrow());
            let id = library.borrow_mut().add_new(name);
            record_added(&history, &library, &id);
            *selected.borrow_mut() = Some(id);
            refresh();
        });
    }
    header.append(&new_btn);

    page.append(&header);
    page.append(&stack);

    // Rename the selected component's card (window-level F2 via the right bar).
    let begin_rename: Rc<dyn Fn()> = {
        let library = Rc::clone(&library);
        let selected = Rc::clone(&selected);
        let card_areas = Rc::clone(&card_areas);
        let refresh_slot = Rc::clone(&refresh_slot);
        let history = Rc::clone(&history);
        Rc::new(move || {
            let Some(id) = selected.borrow().clone() else { return };
            let (idx, name) = {
                let lib = library.borrow();
                let Some(pos) = lib.components.iter().position(|c| c.id == id) else {
                    return;
                };
                (pos, lib.components[pos].name.clone())
            };
            let area = card_areas.borrow().get(idx).cloned();
            // Skip when the card isn't mapped (panel hidden behind the Crop sidebar).
            if let Some(area) = area.filter(gtk::prelude::WidgetExt::is_mapped) {
                show_rename_popover(&area, &id, &name, &library, &refresh_slot, &history);
            }
        })
    };

    refresh();
    (page, refresh, begin_rename)
}

/// Pick a default "Component N" name not already taken.
fn next_component_name(lib: &ComponentLibrary) -> String {
    let mut n = lib.len() + 1;
    loop {
        let candidate = format!("Component {n}");
        if lib.components.iter().all(|c| c.name != candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Pop up a small entry over `parent` to rename the component `id`. Commits on
/// Enter, dismisses on focus-out, and rebuilds the grid so the new name shows.
fn show_rename_popover(
    parent: &gtk::DrawingArea,
    id: &str,
    current_name: &str,
    library: &Rc<RefCell<ComponentLibrary>>,
    refresh_slot: &RefreshSlot,
    history: &Rc<RefCell<HistoryStack>>,
) {
    let entry = gtk::Entry::builder()
        .text(current_name)
        .width_chars(16)
        .build();
    entry.select_region(0, -1);

    let popover = Rc::new(gtk::Popover::new());
    popover.set_child(Some(&entry));
    popover.set_parent(parent);

    {
        let popover = Rc::clone(&popover);
        let library = Rc::clone(library);
        let refresh_slot = Rc::clone(refresh_slot);
        let history = Rc::clone(history);
        let id = id.to_string();
        let old_name = current_name.to_string();
        entry.connect_activate(move |e| {
            let trimmed = e.text().trim().to_string();
            if !trimmed.is_empty() && trimmed != old_name {
                if let Some(c) = library.borrow_mut().get_mut(&id) {
                    c.name.clone_from(&trimmed);
                }
                history.borrow_mut().record(HistoryAction::ComponentRename {
                    id: id.clone(),
                    old_name: old_name.clone(),
                    new_name: trimmed,
                });
            }
            popover.popdown();
            trigger_refresh(&refresh_slot);
        });
    }
    // Dismissal is handled by the popover's autohide (click-away / Escape); a
    // focus-leave handler closes too eagerly when the card isn't focused.

    // Defer the popup so it survives the context menu closing in the same tick:
    // popping up a second autohide popover synchronously gets it dismissed.
    glib::idle_add_local_once(move || {
        popover.popup();
        entry.grab_focus();
    });
}

/// Record a just-added component (New or Duplicate) onto the undo stack.
fn record_added(
    history: &Rc<RefCell<HistoryStack>>,
    library: &Rc<RefCell<ComponentLibrary>>,
    id: &str,
) {
    let lib = library.borrow();
    if let Some(idx) = lib.components.iter().position(|c| c.id == id) {
        let snapshot = lib.components[idx].to_snapshot();
        history
            .borrow_mut()
            .record(HistoryAction::ComponentAdd { index: idx, snapshot });
    }
}

/// Snapshot a component, remove it, and record the removal for undo.
fn remove_and_record(
    history: &Rc<RefCell<HistoryStack>>,
    library: &Rc<RefCell<ComponentLibrary>>,
    id: &str,
) {
    let found = {
        let lib = library.borrow();
        lib.components
            .iter()
            .position(|c| c.id == id)
            .map(|idx| (idx, lib.components[idx].to_snapshot()))
    };
    if let Some((index, snapshot)) = found {
        library.borrow_mut().remove(id);
        history
            .borrow_mut()
            .record(HistoryAction::ComponentRemove { index, snapshot });
    }
}

/// Schedule a grid rebuild on the next idle tick. Deferring keeps us from
/// destroying the very widget whose click/gesture handler is running.
fn trigger_refresh(slot: &RefreshSlot) {
    let slot = Rc::clone(slot);
    glib::idle_add_local_once(move || {
        if let Some(refresh) = slot.borrow().as_ref() {
            refresh();
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn build_card(
    id: &str,
    name: &str,
    master: &[u8],
    cw: u32,
    ch: u32,
    library: &Rc<RefCell<ComponentLibrary>>,
    selected_id: &Rc<RefCell<Option<String>>>,
    on_edit: &Rc<dyn Fn(String)>,
    refresh_slot: &RefreshSlot,
    card_areas: &Rc<RefCell<Vec<gtk::DrawingArea>>>,
    history: &Rc<RefCell<HistoryStack>>,
) -> gtk::Widget {
    // A never-drawn component shows the placeholder glyph instead of its
    // (fully transparent) master.
    let blank = is_blank(master);
    let surface = surface_from_bgra(master, cw, ch);

    let area = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .height_request(CARD_HEIGHT)
        .build();
    {
        let surface = surface.clone();
        // Read the selection live so selecting only needs a redraw, never a
        // widget rebuild (a rebuild mid-gesture would break double-click/drag).
        let selected_id = Rc::clone(selected_id);
        let id_draw = id.to_string();
        area.set_draw_func(move |area, cr, w, h| {
            let selected = selected_id.borrow().as_deref() == Some(id_draw.as_str());
            let wf = f64::from(w);
            let hf = f64::from(h);
            let radius = 10.0;

            // Flat dark card.
            rounded_rect(cr, 0.5, 0.5, wf - 1.0, hf - 1.0, radius);
            cr.set_source_rgb(CARD_BG.0, CARD_BG.1, CARD_BG.2);
            cr.fill().ok();

            if blank || surface.is_none() {
                draw_empty_caption(cr, w, h);
            } else if let Some(surf) = surface.as_ref() {
                cr.save().ok();
                rounded_rect(cr, 0.5, 0.5, wf - 1.0, hf - 1.0, radius);
                cr.clip();
                draw_checker(cr, w, h);
                paint_contained(cr, surf, cw, ch, w, h);
                cr.restore().ok();
            }

            if selected {
                let accent = lookup_accent(area);
                cr.set_source_rgb(accent.0, accent.1, accent.2);
                cr.set_line_width(2.0);
                rounded_rect(cr, 1.0, 1.0, wf - 2.0, hf - 2.0, radius);
            } else {
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.10);
                cr.set_line_width(1.0);
                rounded_rect(cr, 0.5, 0.5, wf - 1.0, hf - 1.0, radius);
            }
            cr.stroke().ok();
        });
    }

    let overlay = gtk::Overlay::builder()
        .height_request(CARD_HEIGHT)
        .hexpand(true)
        .build();
    overlay.set_child(Some(&area));
    card_areas.borrow_mut().push(area.clone());

    // Remove, top-right.
    let remove_btn = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_end(6)
        .margin_top(6)
        .tooltip_text("Remove component")
        .build();
    remove_btn.add_css_class("flat");
    remove_btn.add_css_class("circular");
    {
        let library = Rc::clone(library);
        let selected_id = Rc::clone(selected_id);
        let refresh_slot = Rc::clone(refresh_slot);
        let history = Rc::clone(history);
        let id = id.to_string();
        remove_btn.connect_clicked(move |_| {
            remove_and_record(&history, &library, &id);
            if selected_id.borrow().as_deref() == Some(id.as_str()) {
                *selected_id.borrow_mut() = None;
            }
            trigger_refresh(&refresh_slot);
        });
    }
    overlay.add_overlay(&remove_btn);

    // Drag source: carries the component id so it can be dropped on the canvas.
    let drag = gtk::DragSource::builder()
        .actions(gtk::gdk::DragAction::COPY)
        .build();
    {
        let id = id.to_string();
        drag.connect_prepare(move |_, _, _| {
            Some(gtk::gdk::ContentProvider::for_value(&id.to_value()))
        });
    }
    area.add_controller(drag);

    // Click: single selects (redraw only), double opens for editing.
    let click = gtk::GestureClick::new();
    {
        let selected_id = Rc::clone(selected_id);
        let card_areas = Rc::clone(card_areas);
        let on_edit = Rc::clone(on_edit);
        let id = id.to_string();
        click.connect_pressed(move |_, n_press, _, _| {
            if n_press >= 2 {
                on_edit(id.clone());
            } else {
                *selected_id.borrow_mut() = Some(id.clone());
                // Redraw all cards so the selection border moves - no rebuild,
                // which would destroy this widget mid double-click / drag.
                for a in card_areas.borrow().iter() {
                    a.queue_draw();
                }
            }
        });
    }
    area.add_controller(click);

    // Per-card actions backing the right-click menu (Rename / Duplicate / Delete).
    let actions = gio::SimpleActionGroup::new();
    {
        let library = Rc::clone(library);
        let refresh_slot = Rc::clone(refresh_slot);
        let history = Rc::clone(history);
        let area_w = area.clone();
        let id = id.to_string();
        let act = gio::SimpleAction::new("rename", None);
        act.connect_activate(move |_, _| {
            let current = library
                .borrow()
                .get(&id)
                .map_or_else(String::new, |c| c.name.clone());
            show_rename_popover(&area_w, &id, &current, &library, &refresh_slot, &history);
        });
        actions.add_action(&act);
    }
    {
        let library = Rc::clone(library);
        let selected_id = Rc::clone(selected_id);
        let refresh_slot = Rc::clone(refresh_slot);
        let history = Rc::clone(history);
        let id = id.to_string();
        let act = gio::SimpleAction::new("duplicate", None);
        act.connect_activate(move |_, _| {
            let new_id = library.borrow_mut().duplicate(&id);
            if let Some(new_id) = new_id {
                record_added(&history, &library, &new_id);
                *selected_id.borrow_mut() = Some(new_id);
            }
            trigger_refresh(&refresh_slot);
        });
        actions.add_action(&act);
    }
    {
        let library = Rc::clone(library);
        let selected_id = Rc::clone(selected_id);
        let refresh_slot = Rc::clone(refresh_slot);
        let history = Rc::clone(history);
        let id = id.to_string();
        let act = gio::SimpleAction::new("delete", None);
        act.connect_activate(move |_, _| {
            remove_and_record(&history, &library, &id);
            if selected_id.borrow().as_deref() == Some(id.as_str()) {
                *selected_id.borrow_mut() = None;
            }
            trigger_refresh(&refresh_slot);
        });
        actions.add_action(&act);
    }
    area.insert_action_group("card", Some(&actions));

    // Right-click context menu.
    let menu = gio::Menu::new();
    menu.append(Some("Rename"), Some("card.rename"));
    menu.append(Some("Duplicate"), Some("card.duplicate"));
    menu.append(Some("Delete"), Some("card.delete"));
    let ctx_menu = gtk::PopoverMenu::from_model(Some(&menu));
    ctx_menu.set_parent(&area);
    ctx_menu.set_has_arrow(false);
    {
        let ctx_menu = ctx_menu.clone();
        area.connect_destroy(move |_| ctx_menu.unparent());
    }
    let secondary = gtk::GestureClick::new();
    secondary.set_button(gdk::BUTTON_SECONDARY);
    {
        let selected_id = Rc::clone(selected_id);
        let card_areas = Rc::clone(card_areas);
        let id = id.to_string();
        secondary.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            *selected_id.borrow_mut() = Some(id.clone());
            for a in card_areas.borrow().iter() {
                a.queue_draw();
            }
            #[allow(clippy::cast_possible_truncation)]
            let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
            ctx_menu.set_pointing_to(Some(&rect));
            ctx_menu.popup();
        });
    }
    area.add_controller(secondary);

    // Card sits above its centred title.
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    card.append(&overlay);

    let title = gtk::Label::builder()
        .label(name)
        .halign(gtk::Align::Center)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(14)
        .build();
    card.append(&title);

    card.upcast()
}

/// Build a cairo surface from premultiplied BGRA8 pixels. Returns `None` if the
/// buffer is empty or mis-sized.
fn surface_from_bgra(bgra: &[u8], w: u32, h: u32) -> Option<cairo::ImageSurface> {
    if w == 0 || h == 0 || bgra.len() != (w * h * 4) as usize {
        return None;
    }
    let mut surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32).ok()?;
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data().ok()?;
        let row_bytes = (w * 4) as usize;
        for y in 0..h as usize {
            let src = y * row_bytes;
            let dst = y * stride;
            data[dst..dst + row_bytes].copy_from_slice(&bgra[src..src + row_bytes]);
        }
    }
    surface.mark_dirty();
    Some(surface)
}

/// Paint `surf` (size `cw x ch`) scaled to fit inside `w x h`, centred,
/// preserving aspect ratio, with nearest-neighbour sampling.
fn paint_contained(
    cr: &cairo::Context,
    surf: &cairo::ImageSurface,
    cw: u32,
    ch: u32,
    w: i32,
    h: i32,
) {
    if cw == 0 || ch == 0 {
        return;
    }
    let wf = f64::from(w);
    let hf = f64::from(h);
    let scale = (wf / f64::from(cw)).min(hf / f64::from(ch));
    let dw = f64::from(cw) * scale;
    let dh = f64::from(ch) * scale;
    let ox = (wf - dw) / 2.0;
    let oy = (hf - dh) / 2.0;
    cr.save().ok();
    cr.translate(ox, oy);
    cr.scale(scale, scale);
    if cr.set_source_surface(surf, 0.0, 0.0).is_ok() {
        cr.source().set_filter(cairo::Filter::Nearest);
    }
    cr.paint().ok();
    cr.restore().ok();
}

/// Transparency checker behind a preview. The two greys mirror the canvas
/// checker (0xEB / 0xC7); a smaller cell suits the thumbnail scale.
fn draw_checker(cr: &cairo::Context, w: i32, h: i32) {
    let w = f64::from(w);
    let h = f64::from(h);
    cr.set_source_rgb(0.92, 0.92, 0.92);
    cr.rectangle(0.0, 0.0, w, h);
    cr.fill().ok();
    cr.set_source_rgb(0.78, 0.78, 0.78);
    let mut y = 0.0;
    let mut row = 0;
    while y < h {
        let mut x = if row % 2 == 0 { CHECKER_SIZE } else { 0.0 };
        while x < w {
            cr.rectangle(x, y, CHECKER_SIZE, CHECKER_SIZE);
            x += CHECKER_SIZE * 2.0;
        }
        cr.fill().ok();
        y += CHECKER_SIZE;
        row += 1;
    }
}

/// True when every pixel is fully transparent (a never-drawn component).
fn is_blank(bgra: &[u8]) -> bool {
    bgra.is_empty() || bgra.chunks_exact(4).all(|p| p[3] == 0)
}

/// Centred "Empty" caption shown for never-drawn components.
fn draw_empty_caption(cr: &cairo::Context, w: i32, h: i32) {
    let wf = f64::from(w);
    let hf = f64::from(h);
    cr.set_font_size(13.0);
    if let Ok(ext) = cr.text_extents("Empty") {
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.55);
        cr.move_to(
            (wf - ext.width()) / 2.0 - ext.x_bearing(),
            (hf - ext.height()) / 2.0 - ext.y_bearing(),
        );
        cr.show_text("Empty").ok();
    }
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

#[allow(deprecated)]
fn lookup_accent(widget: &impl IsA<gtk::Widget>) -> (f64, f64, f64) {
    widget
        .style_context()
        .lookup_color("accent_bg_color")
        .or_else(|| widget.style_context().lookup_color("accent_color"))
        .map_or(FALLBACK_ACCENT, |c| {
            (
                f64::from(c.red()),
                f64::from(c.green()),
                f64::from(c.blue()),
            )
        })
}
