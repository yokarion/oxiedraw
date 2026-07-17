//! Widget helpers shared between the picker popover and the brush
//! manager window.
//!
//! Centralised here so the two surfaces always render rows the same
//! way - same icon, same preview, same ellipsised name.

use std::rc::Rc;

use oxiedraw_core::brush_engine::{BrushPreset, PatternData};
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use super::preview;

const ROW_ICON_SIZE: i32 = 60;

/// Build a list row showing the brush's icon, name, and a cairo
/// stroke preview. Suitable for both the popover and manager list.
///
/// `is_default` controls whether the star icon appears filled (yellow) or
/// outline (gray). `on_set_default` is called when the star is clicked;
/// pass `None` to omit the star entirely. The returned `Option<gtk::Button>`
/// is `Some` when a star button was created, so callers can update it later.
pub(crate) fn build_list_row(
    preset: &BrushPreset,
    is_default: bool,
    on_set_default: Option<Rc<dyn Fn()>>,
) -> (gtk::ListBoxRow, Option<gtk::Button>) {
    let row = gtk::ListBoxRow::new();
    let h = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();

    // Star: grayed-out outline when not default, yellow filled when default.
    let star_btn = on_set_default.map(|cb| {
        let star = gtk::Button::builder()
            .has_frame(false)
            .valign(gtk::Align::Center)
            .tooltip_text("Default Brush")
            .build();
        update_star_icon(&star, is_default);
        star.add_css_class("brush-star-btn");
        ensure_star_css();
        star.connect_clicked(move |_| cb());
        h.append(&star);
        star
    });

    ensure_icon_css();
    let icon = gtk::Image::builder().pixel_size(ROW_ICON_SIZE).build();
    icon.add_css_class("brush-row-icon");
    icon.set_overflow(gtk::Overflow::Hidden);
    apply_icon_to_image(&icon, preset, super::FALLBACK_ICON);
    h.append(&icon);

    let label = gtk::Label::builder()
        .label(&preset.name)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .xalign(0.0)
        .hexpand(true)
        .width_request(200)
        .build();
    h.append(&label);

    h.append(&preview::build(preset));
    row.set_child(Some(&h));
    (row, star_btn)
}

/// Set the star icon on the button based on whether the brush is default.
pub(crate) fn update_star_icon(btn: &gtk::Button, is_default: bool) {
    if is_default {
        btn.set_icon_name("starred-symbolic");
        btn.add_css_class("starred");
    } else {
        btn.set_icon_name("non-starred-symbolic");
        btn.remove_css_class("starred");
    }
}

fn ensure_star_css() {
    use std::sync::OnceLock;
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            ".brush-star-btn { opacity: 0.4; }
             .brush-star-btn:hover { opacity: 1.0; }
             .brush-star-btn.starred { opacity: 1.0; color: #f5c518; }",
        );
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

/// Install the rounded-corner style for brush row icons once per
/// process. Pairs with `Overflow::Hidden` on the image so the icon
/// texture is clipped to the rounded rect.
fn ensure_icon_css() {
    use std::sync::OnceLock;
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(".brush-row-icon { border-radius: 4px; }");
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

/// Decode and apply a brush's PNG icon to a `gtk::Image`, falling
/// back to `fallback_icon_name` (a symbolic icon name) on `None` or
/// decode failure.
pub(crate) fn apply_icon_to_image(
    image: &gtk::Image,
    preset: &BrushPreset,
    fallback_icon_name: &str,
) {
    if let Some(bytes) = &preset.icon
        && let Some(texture) = decode_icon_bytes(bytes)
    {
        image.set_paintable(Some(&texture));
        return;
    }
    image.set_icon_name(Some(fallback_icon_name));
}

fn decode_icon_bytes(bytes: &[u8]) -> Option<gtk::gdk::Texture> {
    let glib_bytes = glib::Bytes::from(bytes);
    gtk::gdk::Texture::from_bytes(&glib_bytes).ok()
}

/// Decode a cached preview PNG into a cairo `ImageSurface`.
///
/// `cairo-rs` builds without the optional `png` feature here, so we
/// route the decode through `PatternData::from_png_bytes` (premul RGBA
/// in core) and then swap R<->B so it lines up with cairo's
/// `Format::ARgb32` which is BGRA-premul on little-endian. Returns
/// `None` if decode or surface allocation fails - callers fall back
/// to the Cairo synthesised preview.
pub(crate) fn decode_preview_png(bytes: &[u8]) -> Option<cairo::ImageSurface> {
    let pattern = PatternData::from_png_bytes(bytes).ok()?;
    #[allow(clippy::cast_possible_wrap)]
    let width = pattern.width as i32;
    #[allow(clippy::cast_possible_wrap)]
    let height = pattern.height as i32;
    let stride = cairo::Format::ARgb32.stride_for_width(pattern.width).ok()?;
    let mut bgra = vec![0u8; (stride as usize) * (height as usize)];
    let row_pixels = pattern.width as usize;
    for y in 0..(height as usize) {
        let src_row = &pattern.rgba[y * row_pixels * 4..(y + 1) * row_pixels * 4];
        let dst_row = &mut bgra[y * (stride as usize)..y * (stride as usize) + row_pixels * 4];
        for (src_px, dst_px) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
            dst_px[0] = src_px[2]; // B
            dst_px[1] = src_px[1]; // G
            dst_px[2] = src_px[0]; // R
            dst_px[3] = src_px[3]; // A
        }
    }
    cairo::ImageSurface::create_for_data(bgra, cairo::Format::ARgb32, width, height, stride).ok()
}

/// Paint a cached preview surface into the current cairo context,
/// scaled to fill `target_w` x `target_h` and tinted with `rgb`. Uses
/// the alpha channel as a mask so the result follows the GTK theme
/// foreground regardless of the stored colour.
pub(crate) fn paint_preview_masked(
    cr: &cairo::Context,
    surface: &cairo::ImageSurface,
    target_w: f64,
    target_h: f64,
    rgb: (f64, f64, f64),
) {
    let sw = f64::from(surface.width()).max(1.0);
    let sh = f64::from(surface.height()).max(1.0);
    // Letterbox: preserve aspect ratio so a 320x80 cache doesn't
    // squash into a 180x32 row. The cache is rendered with its own
    // padding so a little extra margin around the stroke is fine.
    let scale = (target_w / sw).min(target_h / sh);
    let draw_w = sw * scale;
    let draw_h = sh * scale;
    let tx = (target_w - draw_w) * 0.5;
    let ty = (target_h - draw_h) * 0.5;

    cr.save().ok();
    cr.translate(tx, ty);
    cr.scale(scale, scale);
    cr.set_source_rgba(rgb.0, rgb.1, rgb.2, 1.0);
    cr.mask_surface(surface, 0.0, 0.0).ok();
    cr.restore().ok();
}
