use std::f64::consts::TAU;

use adw::prelude::*;
use oxiedraw_core::tools::{CropOverlay, CropState};
use relm4::RelmWidgetExt;
use relm4::gtk;
use relm4::gtk::glib;

const PANEL_MARGIN: i32 = 12;

pub(crate) fn build(crop: &CropState) -> gtk::Box {
    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    panel.add_css_class("sidebar");

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(PANEL_MARGIN)
        .margin_bottom(PANEL_MARGIN)
        .margin_start(PANEL_MARGIN)
        .margin_end(PANEL_MARGIN)
        .valign(gtk::Align::Start)
        .build();

    content.append(&build_header(crop));
    content.append(&build_section_label("Overlay"));
    content.append(&build_overlay_row(crop));
    content.append(&build_section_label("Behavior"));
    content.append(&build_behavior_list(crop));

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();
    scroll.set_child(Some(&content));

    panel.append(&scroll);
    panel
}

fn build_header(crop: &CropState) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    let icon = gtk::Image::from_icon_name("oxiedraw-crop-symbolic");
    icon.add_css_class("accent");
    icon.set_pixel_size(18);
    row.append(&icon);

    let title = gtk::Label::builder()
        .label("Crop properties")
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();
    title.inline_css("font-weight: 600;");
    row.append(&title);

    let dim_label = gtk::Label::builder()
        .label("")
        .halign(gtk::Align::End)
        .xalign(1.0)
        // Fixed width: the dimensions update live during a crop drag, and a
        // width change here would relayout up to the canvas Picture, cancelling
        // the in-progress stylus grab. A fixed request keeps the size stable.
        .width_chars(13)
        .max_width_chars(13)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    dim_label.add_css_class("dim-label");
    dim_label.inline_css("font-size: 12px; opacity: 0.6;");
    row.append(&dim_label);

    {
        let crop_c = crop.clone();
        let dim_c = dim_label.clone();
        crop.connect_rect_changed(Box::new(move || {
            let text = crop_c.rect.get().map_or_else(String::new, |r| {
                let n = r.normalized();
                format!("{} x {}", n.width_px(), n.height_px())
            });
            dim_c.set_label(&text);
        }));
    }

    row
}

fn build_section_label(text: &str) -> gtk::Label {
    let lbl = gtk::Label::builder()
        .label(text)
        .halign(gtk::Align::Start)
        .build();
    lbl.add_css_class("heading");
    lbl
}

// ---------------------------------------------------------------------------
// Overlay
// ---------------------------------------------------------------------------

const OVERLAYS: [CropOverlay; 3] = [
    CropOverlay::Thirds,
    CropOverlay::Grid,
    CropOverlay::Diagonal,
];

fn build_overlay_row(crop: &CropState) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .homogeneous(true)
        .valign(gtk::Align::Start) // don't let the row stretch to fill the sidebar height
        .build();

    let buttons: Vec<gtk::ToggleButton> = OVERLAYS
        .iter()
        .map(|&ov| make_overlay_toggle(ov, crop.overlay.get() == ov))
        .collect();

    for btn in buttons.iter().skip(1) {
        btn.set_group(Some(&buttons[0]));
    }

    for (btn, &ov) in buttons.iter().zip(OVERLAYS.iter()) {
        {
            let crop_c = crop.clone();
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    crop_c.overlay.set(ov);
                    crop_c.notify_rect_changed(); // sync overlay to paintable
                }
            });
        }
        btn.set_hexpand(true);
        // Each frame: if the allocated width changed, update the height request
        // to match so the button stays square.
        btn.add_tick_callback(|b, _| {
            let w = b.width();
            if w > 0 && b.height_request() != w {
                b.set_size_request(-1, w);
            }
            glib::ControlFlow::Continue
        });
        row.append(btn);
    }

    row
}

fn make_overlay_toggle(ov: CropOverlay, active: bool) -> gtk::ToggleButton {
    let drawing = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .can_target(false)
        .build();
    drawing.set_draw_func(move |_, cr, w, h| draw_overlay_icon(cr, w, h, ov));

    let label = gtk::Label::builder().label(ov.display_name()).build();
    label.inline_css("font-size: 9px; font-weight: 600;");

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    inner.append(&drawing);
    inner.append(&label);

    let btn = gtk::ToggleButton::builder().active(active).build();
    btn.set_child(Some(&inner));
    btn.add_css_class("flat");
    btn.inline_css("border-radius: 8px;");
    btn
}

// ---------------------------------------------------------------------------
// Behavior
// ---------------------------------------------------------------------------

fn build_behavior_list(crop: &CropState) -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);

    let row = adw::ActionRow::builder()
        .title("Snap to Canvas")
        .subtitle("Edges snap to Canvas Borders")
        .build();

    let toggle = gtk::Switch::builder()
        .active(crop.snap_to_canvas.get())
        .valign(gtk::Align::Center)
        .build();
    {
        let crop_c = crop.clone();
        toggle.connect_active_notify(move |sw| {
            crop_c.snap_to_canvas.set(sw.is_active());
        });
    }
    row.add_suffix(&toggle);
    row.set_activatable_widget(Some(&toggle));
    list.append(&row);

    list
}

// ---------------------------------------------------------------------------
// Overlay icon drawing
// ---------------------------------------------------------------------------

fn draw_overlay_icon(cr: &gtk::cairo::Context, w: i32, h: i32, ov: CropOverlay) {
    let wf = f64::from(w);
    let hf = f64::from(h);

    cr.set_source_rgba(0.5, 0.5, 0.5, 0.4);
    cr.set_line_width(1.0);
    cr.rectangle(4.0, 4.0, wf - 8.0, hf - 8.0);
    cr.stroke().ok();

    cr.set_source_rgba(0.5, 0.5, 0.5, 0.7);
    cr.set_line_width(0.8);

    let x1 = 4.0;
    let y1 = 4.0;
    let x2 = wf - 4.0;
    let y2 = hf - 4.0;
    let bw = x2 - x1;
    let bh = y2 - y1;

    match ov {
        CropOverlay::Thirds => {
            for i in 1..3 {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f64 / 3.0;
                cr.move_to(x1 + bw * t, y1);
                cr.line_to(x1 + bw * t, y2);
                cr.move_to(x1, y1 + bh * t);
                cr.line_to(x2, y1 + bh * t);
            }
            cr.stroke().ok();
        }
        CropOverlay::Grid => {
            for i in 1..4 {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f64 / 4.0;
                cr.move_to(x1 + bw * t, y1);
                cr.line_to(x1 + bw * t, y2);
                cr.move_to(x1, y1 + bh * t);
                cr.line_to(x2, y1 + bh * t);
            }
            cr.stroke().ok();
        }
        CropOverlay::Diagonal => {
            cr.move_to(x1, y1);
            cr.line_to(x2, y2);
            cr.move_to(x2, y1);
            cr.line_to(x1, y2);
            cr.stroke().ok();
        }
        CropOverlay::Triangle => {
            cr.move_to(x1, y2);
            cr.line_to(x2, y1);
            let mx = f64::midpoint(x1, x2);
            cr.move_to(x1, y1);
            cr.line_to(mx, y2);
            cr.stroke().ok();
        }
        CropOverlay::Golden => {
            let g = 0.382;
            cr.move_to(x1 + bw * g, y1);
            cr.line_to(x1 + bw * g, y2);
            cr.move_to(x1 + bw * (1.0 - g), y1);
            cr.line_to(x1 + bw * (1.0 - g), y2);
            cr.move_to(x1, y1 + bh * g);
            cr.line_to(x2, y1 + bh * g);
            cr.move_to(x1, y1 + bh * (1.0 - g));
            cr.line_to(x2, y1 + bh * (1.0 - g));
            cr.stroke().ok();
        }
        CropOverlay::Spiral => {
            let cx = f64::midpoint(x1, x2);
            let cy = f64::midpoint(y1, y2);
            let max_r = bw.min(bh) / 2.0;
            let mut first = true;
            let steps = 60;
            for i in 0..=steps {
                #[allow(clippy::cast_precision_loss)]
                let angle = i as f64 / steps as f64 * TAU * 1.5;
                let r = max_r * (1.0 - angle / (TAU * 1.5));
                let px = cx + r * angle.cos();
                let py = cy + r * angle.sin();
                if first {
                    cr.move_to(px, py);
                    first = false;
                } else {
                    cr.line_to(px, py);
                }
            }
            cr.stroke().ok();
        }
    }
}
