//! Helpers for the libadwaita "boxed list" (linked list) look: a `GtkListBox`
//! with the `boxed-list` style class holding non-selectable rows, each a title
//! label on the left and a control filling the rest. Used by the filter popups
//! and the adjustment-effect editor so their settings share one style.

use relm4::gtk;
use relm4::gtk::prelude::*;

const TITLE_WIDTH: i32 = 110;

/// A `GtkListBox` styled as a libadwaita boxed list.
pub(crate) fn list() -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    list
}

/// One boxed-list row: `title` on the left, `control` filling the remaining
/// width, then any `suffixes` (e.g. a lock toggle) trailing it.
pub(crate) fn row(
    title: &str,
    control: &impl IsA<gtk::Widget>,
    suffixes: &[&gtk::Widget],
) -> gtk::ListBoxRow {
    let hbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    let lbl = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .width_request(TITLE_WIDTH)
        .build();
    hbox.append(&lbl);
    let w = control.as_ref();
    w.set_hexpand(true);
    hbox.append(w);
    for s in suffixes {
        hbox.append(*s);
    }
    gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .child(&hbox)
        .build()
}

/// A plain (non-interactive) informational row holding a single dimmed label -
/// used for parameterless effects so an expanded panel is never empty.
pub(crate) fn info_row(text: &str) -> gtk::ListBoxRow {
    let lbl = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    lbl.add_css_class("dim-label");
    gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .child(&lbl)
        .build()
}
