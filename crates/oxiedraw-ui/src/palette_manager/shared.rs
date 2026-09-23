//! Pieces both palette windows are built from, so Extract Palette and Manage
//! Palettes lay out the same way: a scrolling body of preference groups, a
//! Colors group holding the swatch grid, and a preview strip pinned under the
//! body like the stroke preview in Manage Brushes.

use adw::prelude::*;
use oxiedraw_core::color::Color;
use relm4::gtk;

use crate::widgets::color_strip::ColorStrip;
use crate::widgets::swatch_grid::SwatchGrid;

const PREVIEW_HEIGHT: i32 = 46;

pub(crate) fn body() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(24)
        .margin_end(24)
        .build()
}

pub(crate) fn scroller(body: &gtk::Box) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(body)
        .build()
}

/// The grid in a boxed-list card under a "Colors" heading. The count, or
/// anything else the window wants beside the heading, goes in `suffix`.
pub(crate) fn colors_group(grid: &SwatchGrid, suffix: Option<&gtk::Widget>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Colors").build();
    if let Some(suffix) = suffix {
        group.set_header_suffix(Some(suffix));
    }
    let padding = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    padding.append(&grid.widget());
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(
        &gtk::ListBoxRow::builder()
            .activatable(false)
            .selectable(false)
            .child(&padding)
            .build(),
    );
    group.add(&list);
    group
}

/// The palette as one rounded ramp, pinned at the bottom of a window.
pub(crate) struct PalettePreview {
    strip: ColorStrip,
    root: gtk::Box,
}

impl PalettePreview {
    pub(crate) fn new() -> Self {
        let strip = ColorStrip::filling(PREVIEW_HEIGHT);
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(8)
            .margin_bottom(12)
            .margin_start(24)
            .margin_end(24)
            .build();
        root.append(&strip.widget());
        Self { strip, root }
    }

    pub(crate) fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    pub(crate) fn set_colors(&self, colors: &[Color]) {
        self.strip.set_colors(colors);
    }

    /// Bands sized by share, for the extractor's frequency-weighted result.
    pub(crate) fn set_weighted(&self, bands: &[(Color, f32)]) {
        self.strip.set_weighted(bands);
    }
}
