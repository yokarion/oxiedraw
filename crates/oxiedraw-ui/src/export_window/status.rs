//! Format-agnostic status labels and value formatters.

use oxiedraw_core::export::estimate_size_bytes;
use oxiedraw_core::export::settings::{ExportFormat, ExportSettings};
use relm4::RelmWidgetExt as _;
use relm4::gtk;
use relm4::gtk::prelude::*;

pub(super) fn section_label(text: &str) -> gtk::Label {
    let lbl = gtk::Label::new(Some(text));
    lbl.set_halign(gtk::Align::Start);
    lbl.add_css_class("dim-label");
    lbl.inline_css("font-size: 11px; font-weight: bold;");
    lbl
}

pub(super) fn format_scale(v: f32) -> String {
    if (v - v.floor()).abs() < 0.001 {
        format!("{v:.0}x")
    } else if ((v * 10.0) - (v * 10.0).floor()).abs() < 0.01 {
        format!("{v:.1}x")
    } else {
        format!("{v:.3}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
            + "x"
    }
}

pub(super) fn format_mime(fmt: ExportFormat) -> &'static str {
    match fmt {
        ExportFormat::Png => "image/png",
        ExportFormat::Jpeg => "image/jpeg",
        ExportFormat::Webp => "image/webp",
        ExportFormat::Avif => "image/avif",
    }
}

#[allow(clippy::cast_precision_loss)]
pub(super) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub(super) fn update_status_labels(
    dim_label: &gtk::Label,
    fmt_label: &gtk::Label,
    size_label: &gtk::Label,
    canvas_w: u32,
    canvas_h: u32,
    settings: &ExportSettings,
) {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let dw = ((canvas_w as f32 * settings.scale).round() as u32).max(1);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let dh = ((canvas_h as f32 * settings.scale).round() as u32).max(1);
    dim_label.set_label(&format!("{dw} x {dh} px"));

    let (fmt_str, color_str, depth_str) = match settings.format {
        ExportFormat::Png => {
            let color = if settings.png.transparency {
                "RGBA"
            } else {
                "RGB"
            };
            let depth = settings.png.bit_depth.label();
            ("PNG", color, depth)
        }
        ExportFormat::Jpeg => ("JPEG", "RGB", "8-bit"),
        ExportFormat::Webp => {
            let color = if settings.webp.transparency {
                "RGBA"
            } else {
                "RGB"
            };
            ("WebP", color, "8-bit")
        }
        ExportFormat::Avif => {
            let color = if settings.avif.transparency {
                "RGBA"
            } else {
                "RGB"
            };
            ("AVIF", color, "8-bit")
        }
    };
    fmt_label.set_label(&format!("{fmt_str} - {color_str} - {depth_str}"));

    let bytes = estimate_size_bytes(canvas_w, canvas_h, settings);
    size_label.set_label(&format!("Est. size {}", format_bytes(bytes)));
}
