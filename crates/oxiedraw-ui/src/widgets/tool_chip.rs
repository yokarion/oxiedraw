use std::rc::Rc;

use oxiedraw_core::tools::Tool;
use relm4::RelmWidgetExt;
use relm4::gtk;
use relm4::gtk::prelude::*;

/// Small rounded badge showing the active tool's icon and name.
///
/// Returned updater swaps the icon and label in-place when the tool changes.
pub(crate) fn build(tool: Tool) -> (gtk::Box, Rc<dyn Fn(Tool)>) {
    let chip = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(5)
        .valign(gtk::Align::Center)
        .margin_start(6)
        .margin_end(6)
        .build();
    chip.inline_css(
        "border-radius: 8px; \
         background: alpha(@accent_color, 0.18); \
         padding: 3px 8px 3px 6px;",
    );

    let icon = gtk::Image::from_icon_name(tool.icon_name());
    icon.add_css_class("accent");
    icon.set_pixel_size(16);
    chip.append(&icon);

    let label = gtk::Label::new(Some(tool.display_name()));
    label.add_css_class("accent");
    label.inline_css("font-weight: 600; font-size: 13px;");
    chip.append(&label);

    let updater: Rc<dyn Fn(Tool)> = {
        let icon = icon.clone();
        let label = label.clone();
        Rc::new(move |t: Tool| {
            icon.set_icon_name(Some(t.icon_name()));
            label.set_label(t.display_name());
        })
    };

    (chip, updater)
}
