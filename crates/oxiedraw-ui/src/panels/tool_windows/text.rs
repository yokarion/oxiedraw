// Hidden when no box is being edited, which takes its floating frame with it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use oxiedraw_core::text::fonts::TextEngine;
use oxiedraw_core::text::{HAlign, ResizeMode, VAlign};
use relm4::RelmWidgetExt;
use relm4::gtk;

use crate::font_previews::FontPreviews;
use crate::text_edit::TextEdit;

type Slot = Rc<RefCell<Option<TextEdit>>>;

const FACE_OPTIONS: [(&str, bool, bool); 4] = [
    ("Regular", false, false),
    ("Bold", true, false),
    ("Italic", false, true),
    ("Bold Italic", true, true),
];

pub(crate) fn build(
    slot: &Slot,
    engine: &Rc<RefCell<TextEngine>>,
    previews: &FontPreviews,
) -> (gtk::Box, Rc<dyn Fn()>) {
    let syncing = Rc::new(Cell::new(false));

    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(10)
        .margin_end(10)
        .build();

    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let icon = gtk::Image::from_icon_name("oxiedraw-text-symbolic");
    icon.add_css_class("accent");
    icon.set_pixel_size(18);
    let title = gtk::Label::builder()
        .label("Editing text")
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();
    title.inline_css("font-weight: 600;");
    header.append(&icon);
    header.append(&title);
    panel.append(&header);

    let families: Vec<String> = engine.borrow().available_families();

    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    panel.append(&list);

    let font_dropdown = build_font_dropdown(&families, previews);
    font_dropdown.set_hexpand(true);
    let font_row = gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .child(&font_dropdown)
        .build();
    font_dropdown.set_margin_top(6);
    font_dropdown.set_margin_bottom(6);
    font_dropdown.set_margin_start(8);
    font_dropdown.set_margin_end(8);
    list.append(&font_row);
    {
        let slot = Rc::clone(slot);
        let syncing = Rc::clone(&syncing);
        let families = families.clone();
        font_dropdown.connect_selected_notify(move |d| {
            if syncing.get() {
                return;
            }
            if let (Some(name), Some(te)) =
                (families.get(d.selected() as usize), slot.borrow().as_ref())
            {
                te.set_font(name.clone());
            }
        });
    }

    let size_spin = gtk::SpinButton::with_range(1.0, 500.0, 1.0);
    size_spin.set_digits(0);
    size_spin.set_value(20.0);
    list.append(&action_row("Font Size", &size_spin));
    {
        let slot = Rc::clone(slot);
        let syncing = Rc::clone(&syncing);
        size_spin.connect_value_changed(move |s| {
            if syncing.get() {
                return;
            }
            if let Some(te) = slot.borrow().as_ref() {
                #[allow(clippy::cast_possible_truncation)]
                te.set_size(s.value() as f32);
            }
        });
    }

    let face_dropdown =
        gtk::DropDown::from_strings(&FACE_OPTIONS.iter().map(|f| f.0).collect::<Vec<_>>());
    list.append(&action_row("Font Family", &face_dropdown));
    {
        let slot = Rc::clone(slot);
        let syncing = Rc::clone(&syncing);
        face_dropdown.connect_selected_notify(move |d| {
            if syncing.get() {
                return;
            }
            let (_, bold, italic) = FACE_OPTIONS[d.selected() as usize % FACE_OPTIONS.len()];
            if let Some(te) = slot.borrow().as_ref() {
                te.set_face(bold, italic);
            }
        });
    }

    let (h_box, h_buttons) = segmented(&[
        ("format-justify-left-symbolic", "Left"),
        ("format-justify-center-symbolic", "Center"),
        ("format-justify-right-symbolic", "Right"),
    ]);
    list.append(&action_row("Horizontal Align", &h_box));
    let h_aligns = [HAlign::Left, HAlign::Center, HAlign::Right];
    for (i, btn) in h_buttons.iter().enumerate() {
        let slot = Rc::clone(slot);
        let syncing = Rc::clone(&syncing);
        let align = h_aligns[i];
        btn.connect_toggled(move |b| {
            if syncing.get() || !b.is_active() {
                return;
            }
            if let Some(te) = slot.borrow().as_ref() {
                te.set_h_align(align);
            }
        });
    }

    let (v_box, v_buttons) = segmented(&[
        ("oxiedraw-text-valign-top-symbolic", "Top"),
        ("oxiedraw-text-valign-middle-symbolic", "Middle"),
        ("oxiedraw-text-valign-bottom-symbolic", "Bottom"),
    ]);
    list.append(&action_row("Vertical Align", &v_box));
    let v_aligns = [VAlign::Top, VAlign::Middle, VAlign::Bottom];
    for (i, btn) in v_buttons.iter().enumerate() {
        let slot = Rc::clone(slot);
        let syncing = Rc::clone(&syncing);
        let align = v_aligns[i];
        btn.connect_toggled(move |b| {
            if syncing.get() || !b.is_active() {
                return;
            }
            if let Some(te) = slot.borrow().as_ref() {
                te.set_v_align(align);
            }
        });
    }

    let (r_box, r_buttons) = segmented(&[
        ("oxiedraw-text-autowidth-symbolic", "Auto Width"),
        ("oxiedraw-text-autoheight-symbolic", "Auto Height"),
        ("oxiedraw-text-fixed-symbolic", "Fixed Size"),
    ]);
    list.append(&action_row("Resizing", &r_box));
    let modes = [ResizeMode::AutoWidth, ResizeMode::AutoHeight, ResizeMode::Fixed];
    for (i, btn) in r_buttons.iter().enumerate() {
        let slot = Rc::clone(slot);
        let syncing = Rc::clone(&syncing);
        let mode = modes[i];
        btn.connect_toggled(move |b| {
            if syncing.get() || !b.is_active() {
                return;
            }
            if let Some(te) = slot.borrow().as_ref() {
                te.set_resize_mode(mode);
            }
        });
    }

    panel.set_visible(false);

    let refresh: Rc<dyn Fn()> = {
        let root = panel.clone();
        let slot = Rc::clone(slot);
        let syncing = Rc::clone(&syncing);
        let families = families.clone();
        Rc::new(move || {
            let props = slot.borrow().as_ref().and_then(TextEdit::props);
            let Some(p) = props else {
                root.set_visible(false);
                return;
            };
            syncing.set(true);
            root.set_visible(true);

            if let Some(idx) = families.iter().position(|f| *f == p.font) {
                #[allow(clippy::cast_possible_truncation)]
                font_dropdown.set_selected(idx as u32);
            }
            size_spin.set_value(f64::from(p.size.round()));
            let face_idx = FACE_OPTIONS
                .iter()
                .position(|&(_, b, i)| b == p.bold && i == p.italic)
                .unwrap_or(0);
            #[allow(clippy::cast_possible_truncation)]
            face_dropdown.set_selected(face_idx as u32);

            let h_idx = h_aligns.iter().position(|&a| a == p.h_align).unwrap_or(0);
            h_buttons[h_idx].set_active(true);
            let v_idx = v_aligns.iter().position(|&a| a == p.v_align).unwrap_or(0);
            v_buttons[v_idx].set_active(true);
            let r_idx = modes.iter().position(|&m| m == p.resize).unwrap_or(0);
            r_buttons[r_idx].set_active(true);

            syncing.set(false);
        })
    };

    (panel, refresh)
}

fn action_row(title: &str, control: &impl IsA<gtk::Widget>) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let control = control.as_ref();
    control.set_valign(gtk::Align::Center);
    row.add_suffix(control);
    row
}

fn segmented(items: &[(&str, &str)]) -> (gtk::Box, Vec<gtk::ToggleButton>) {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .build();
    let mut buttons = Vec::with_capacity(items.len());
    let mut first: Option<gtk::ToggleButton> = None;
    for &(icon, tooltip) in items {
        let btn = gtk::ToggleButton::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .build();
        if let Some(ref f) = first {
            btn.set_group(Some(f));
        } else {
            first = Some(btn.clone());
        }
        row.append(&btn);
        buttons.push(btn);
    }
    (row, buttons)
}

fn build_font_dropdown(families: &[String], previews: &FontPreviews) -> gtk::DropDown {
    let refs: Vec<&str> = families.iter().map(String::as_str).collect();
    let model = gtk::StringList::new(&refs);

    let expr = gtk::PropertyExpression::new(
        gtk::StringObject::static_type(),
        None::<gtk::Expression>,
        "string",
    );
    let dropdown = gtk::DropDown::new(Some(model), Some(expr));
    dropdown.set_enable_search(true);
    // MUST come after enabling search, or GtkDropDown keeps its text factory.
    dropdown.set_factory(Some(&preview_factory(previews)));
    dropdown
}

fn preview_factory(previews: &FontPreviews) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let picture = gtk::Picture::builder()
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Center)
            .content_fit(gtk::ContentFit::ScaleDown)
            .can_shrink(true)
            .build();
        picture.set_size_request(-1, 20);
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .visible(false)
            .build();
        row.append(&picture);
        row.append(&label);
        item.downcast_ref::<gtk::ListItem>()
            .expect("ListItem")
            .set_child(Some(&row));
    });
    let previews = previews.clone();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let Some(obj) = item.item() else { return };
        let name = obj
            .downcast::<gtk::StringObject>()
            .expect("StringObject")
            .string();
        let row = item
            .child()
            .and_then(|c| c.downcast::<gtk::Box>().ok())
            .expect("Box");
        let picture = row.first_child().and_downcast::<gtk::Picture>().expect("Picture");
        let label = picture.next_sibling().and_downcast::<gtk::Label>().expect("Label");
        if let Some(tex) = previews.get(&name) {
            picture.set_paintable(Some(&tex));
            picture.set_visible(true);
            label.set_visible(false);
        } else {
            label.set_text(&name);
            label.set_visible(true);
            picture.set_visible(false);
        }
        row.set_tooltip_text(Some(&name));
    });
    factory
}
