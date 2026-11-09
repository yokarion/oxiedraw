//! Cairo paths and surfaces shared by the drawn widgets.

use relm4::gtk::cairo;

/// Trace a rounded rectangle as the current path. The radius is clamped to
/// what the box can hold, so a squat rectangle rounds to a stadium.
pub(crate) fn rounded_rect(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    radius: f64,
) {
    let radius = radius.max(0.0).min(width / 2.0).min(height / 2.0);
    let turn = std::f64::consts::TAU;
    cr.new_path();
    cr.arc(x + radius, y + radius, radius, turn / 2.0, turn * 0.75);
    cr.arc(x + width - radius, y + radius, radius, turn * 0.75, turn);
    cr.arc(x + width - radius, y + height - radius, radius, 0.0, turn / 4.0);
    cr.arc(x + radius, y + height - radius, radius, turn / 4.0, turn / 2.0);
    cr.close_path();
}

#[allow(clippy::cast_possible_truncation)]
pub(crate) fn draw_checker(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, cell: f64) {
    cr.save().ok();
    cr.rectangle(x, y, w, h);
    cr.clip();
    cr.rectangle(x, y, w, h);
    cr.set_source_rgb(0.6, 0.6, 0.6);
    cr.fill().ok();
    let cols = (w / cell).ceil() as i32;
    let rows = (h / cell).ceil() as i32;
    cr.set_source_rgb(0.85, 0.85, 0.85);
    for row in 0..rows {
        for col in 0..cols {
            if (row + col) % 2 == 0 {
                cr.rectangle(x + f64::from(col) * cell, y + f64::from(row) * cell, cell, cell);
                cr.fill().ok();
            }
        }
    }
    cr.restore().ok();
}

/// Premultiplied BGRA8 rows copied into an ARGB32 surface, honouring its stride.
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
pub(crate) fn surface_from_bgra(pixels: &[u8], width: u32, height: u32) -> Option<cairo::ImageSurface> {
    let row = width as usize * 4;
    if width == 0 || height == 0 || pixels.len() < row * height as usize {
        return None;
    }
    let mut surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, width as i32, height as i32).ok()?;
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data().ok()?;
        for (dst, src) in data.chunks_mut(stride).zip(pixels.chunks(row)) {
            dst[..row].copy_from_slice(src);
        }
    }
    surface.mark_dirty();
    Some(surface)
}
