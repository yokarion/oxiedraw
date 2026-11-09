//! Blocking choice shown before merging a folder whose layers use a non-Normal
//! blend mode: a side-by-side preview of both ways to bake it.

use std::rc::Rc;

use adw::prelude::*;
use relm4::gtk;
use relm4::gtk::cairo;

use crate::widgets::shapes::{draw_checker, surface_from_bgra};

const CONTENT_MARGIN: i32 = 16;
const CHECKER_CELL: f64 = 12.0;
const MAX_PREVIEW_EDGE: f64 = 1024.0;
const HINT: &str = "After Merge, it can look different. You can automatically change them to \
                    Normal or apply overlay only to the overlapping layers.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MergeBlend {
    ChangeToNormal,
    ApplyOverlapping,
}

pub(super) struct MergePreviews<'a> {
    pub as_normal: &'a [u8],
    pub overlapping: &'a [u8],
    pub width: u32,
    pub height: u32,
}

pub(super) fn show(
    parent: Option<&gtk::Window>,
    message: &str,
    previews: &MergePreviews<'_>,
    on_choice: Rc<dyn Fn(MergeBlend)>,
) {
    let (parent_w, parent_h) = parent.map_or((1280, 800), |p| (p.width(), p.height()));
    let window = adw::Window::builder()
        .modal(true)
        .title("Merge Group")
        .default_width(fit_dimension(parent_w, 560, 960))
        .default_height(fit_dimension(parent_h, 400, 680))
        .resizable(true)
        .build();
    window.set_transient_for(parent);

    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .show_start_title_buttons(false)
        .build();
    let cancel_btn = gtk::Button::with_label("Cancel");
    header.pack_start(&cancel_btn);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(CONTENT_MARGIN)
        .margin_bottom(CONTENT_MARGIN)
        .margin_start(CONTENT_MARGIN)
        .margin_end(CONTENT_MARGIN)
        .build();
    let label = gtk::Label::builder()
        .label(message)
        .use_markup(true)
        .wrap(true)
        .xalign(0.0)
        .build();
    content.append(&label);
    let hint = gtk::Label::builder()
        .label(HINT)
        .wrap(true)
        .xalign(0.0)
        .margin_top(8)
        .css_classes(["dim-label"])
        .build();
    content.append(&hint);

    let split = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .homogeneous(true)
        .vexpand(true)
        .build();
    let choices = [
        (MergeBlend::ChangeToNormal, "Change to Normal", previews.as_normal),
        (MergeBlend::ApplyOverlapping, "Apply Overlapping", previews.overlapping),
    ];
    for (choice, title, pixels) in choices {
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        let preview = preview_area(pixels, previews.width, previews.height);
        column.append(&preview);
        let button = gtk::Button::with_label(title);
        button.add_css_class("suggested-action");
        let window_weak = window.downgrade();
        let on_choice = Rc::clone(&on_choice);
        button.connect_clicked(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.close();
            }
            on_choice(choice);
        });
        column.append(&button);
        split.append(&column);
    }
    content.append(&split);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    {
        let window_weak = window.downgrade();
        cancel_btn.connect_clicked(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.close();
            }
        });
    }
    let key = gtk::EventControllerKey::new();
    key.connect_key_pressed(|controller, keyval, _, _| {
        if keyval != gtk::gdk::Key::Escape {
            return gtk::glib::Propagation::Proceed;
        }
        if let Some(window) = controller.widget().and_downcast::<gtk::Window>() {
            window.close();
        }
        gtk::glib::Propagation::Stop
    });
    window.add_controller(key);
    window.present();
}

fn fit_dimension(parent: i32, min: i32, max: i32) -> i32 {
    (parent * 3 / 5).clamp(min.min(parent.max(1)), max)
}

fn preview_area(pixels: &[u8], width: u32, height: u32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .build();
    area.add_css_class("card");
    let surface = surface_from_bgra(pixels, width, height).and_then(|full| shrink(&full));
    area.set_draw_func(move |_, cr, w, h| {
        let Some(surface) = surface.as_ref() else { return };
        let (sw, sh) = (f64::from(surface.width()), f64::from(surface.height()));
        let scale = (f64::from(w) / sw).min(f64::from(h) / sh);
        let (dw, dh) = (sw * scale, sh * scale);
        let x = (f64::from(w) - dw) / 2.0;
        let y = (f64::from(h) - dh) / 2.0;
        draw_checker(cr, x, y, dw, dh, CHECKER_CELL);
        cr.save().ok();
        cr.translate(x, y);
        cr.scale(scale, scale);
        cr.set_source_surface(surface, 0.0, 0.0).ok();
        cr.source().set_filter(cairo::Filter::Good);
        cr.paint().ok();
        cr.restore().ok();
    });
    area
}

// Downscales once so redraws do not filter the full canvas each time.
#[allow(clippy::cast_possible_truncation)]
fn shrink(full: &cairo::ImageSurface) -> Option<cairo::ImageSurface> {
    let (fw, fh) = (f64::from(full.width()), f64::from(full.height()));
    let scale = (MAX_PREVIEW_EDGE / fw.max(fh)).min(1.0);
    let w = (fw * scale).round().max(1.0) as i32;
    let h = (fh * scale).round().max(1.0) as i32;
    let small = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
    {
        let cr = cairo::Context::new(&small).ok()?;
        cr.scale(f64::from(w) / fw, f64::from(h) / fh);
        cr.set_source_surface(full, 0.0, 0.0).ok()?;
        cr.source().set_filter(cairo::Filter::Good);
        cr.paint().ok()?;
    }
    Some(small)
}
