//! Cairo preview surface + drawing helpers (zoom, pan, alpha checker).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_core::export::settings::{ExportFormat, ExportSettings};
use relm4::gtk;

pub(super) fn build_preview_surface(
    pixels: &[u8],
    w: u32,
    h: u32,
    surface_ref: &Rc<RefCell<Option<gtk::cairo::ImageSurface>>>,
) {
    use gtk::cairo;
    let mut surface = match cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "export preview: create ImageSurface failed");
            return;
        }
    };
    {
        let stride = surface.stride() as usize;
        if let Ok(mut data) = surface.data() {
            for row in 0..h as usize {
                let src = row * w as usize * 4;
                let dst = row * stride;
                data[dst..dst + w as usize * 4].copy_from_slice(&pixels[src..src + w as usize * 4]);
            }
        }
    }
    *surface_ref.borrow_mut() = Some(surface);
}

pub(super) fn format_alpha(s: &ExportSettings) -> bool {
    match s.format {
        ExportFormat::Png => s.png.transparency,
        ExportFormat::Webp => s.webp.transparency,
        ExportFormat::Avif => s.avif.transparency,
        ExportFormat::Jpeg => false,
    }
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub(super) fn draw_preview(
    cr: &gtk::cairo::Context,
    widget_w: i32,
    widget_h: i32,
    surface_ref: &Rc<RefCell<Option<gtk::cairo::ImageSurface>>>,
    zoom: &Rc<Cell<f64>>,
    pan: &Rc<Cell<(f64, f64)>>,
    show_alpha: bool,
) {
    use gtk::cairo;

    let wf = widget_w as f64;
    let hf = widget_h as f64;

    if show_alpha {
        draw_checkerboard(cr, wf, hf);
    } else {
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let _ = cr.paint();
    }

    let surface = surface_ref.borrow();
    let Some(ref surf) = *surface else { return };

    let sw = surf.width() as f64;
    let sh = surf.height() as f64;
    let fit = (wf / sw).min(hf / sh);
    let display_scale = fit * zoom.get();

    let (px, py) = pan.get();
    let tx = (wf - sw * display_scale) / 2.0 + px;
    let ty = (hf - sh * display_scale) / 2.0 + py;

    let _ = cr.save();
    cr.translate(tx, ty);
    cr.scale(display_scale, display_scale);
    let _ = cr.set_source_surface(surf, 0.0, 0.0);
    cr.source().set_filter(cairo::Filter::Bilinear);
    let _ = cr.paint();
    let _ = cr.restore();
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn draw_checkerboard(cr: &gtk::cairo::Context, w: f64, h: f64) {
    let tile = 16.0_f64;
    let light = 0.82_f64;
    let dark = 0.68_f64;

    let cols = (w / tile).ceil() as i32 + 1;
    let rows = (h / tile).ceil() as i32 + 1;

    for row in 0..rows {
        for col in 0..cols {
            let c = if (row + col) % 2 == 0 { light } else { dark };
            cr.set_source_rgb(c, c, c);
            let () = cr.rectangle(col as f64 * tile, row as f64 * tile, tile, tile);
            let _ = cr.fill();
        }
    }
}
