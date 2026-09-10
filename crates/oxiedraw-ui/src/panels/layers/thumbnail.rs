
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use oxiedraw_core::canvas::Canvas;
use oxiedraw_utils::frame_profile;
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use super::{SWATCH_SIZE, Ui};

pub(super) fn start_thumbnail_refresh(
    ui: &Ui,
    canvas: Rc<RefCell<Canvas>>,
    area: gtk::DrawingArea,
) {
    let ui = ui.clone();
    let last_ver = Rc::new(Cell::new(0u64));
    // Only changed layers are re-read: the GPU readback is the expensive part.
    let mut last_layer_vers: Vec<u64> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        let _span = frame_profile::span(frame_profile::Stage::Timers);
        if canvas.borrow().is_drawing() {
            return glib::ControlFlow::Continue;
        }
        let current_ver = canvas.borrow().pixels_version();
        if current_ver == last_ver.get() {
            return glib::ControlFlow::Continue;
        }
        let size = canvas.borrow().size();
        let count = ui.state.len();
        // A changed layer count invalidates the index-to-version mapping.
        let structure_changed = last_layer_vers.len() != count;
        let mut thumbs = ui.thumbnails.borrow_mut();
        thumbs.resize_with(count, || None);
        last_layer_vers.resize(count, u64::MAX);
        let mut any = false;
        for idx in 0..count {
            let ver = canvas.borrow().layer_content_version(idx);
            if !structure_changed && ver == last_layer_vers[idx] && thumbs[idx].is_some() {
                continue;
            }
            match canvas.borrow_mut().read_layer_into(idx, &mut scratch) {
                Ok(()) => {
                    thumbs[idx] = Some(make_thumbnail(&scratch, size.width, size.height));
                    last_layer_vers[idx] = ver;
                    any = true;
                }
                Err(e) => tracing::warn!(idx, error = %e, "layer thumbnail read failed"),
            }
        }
        drop(thumbs);
        last_ver.set(current_ver);
        if any {
            area.queue_draw();
        }
        glib::ControlFlow::Continue
    });
}

pub(super) fn make_thumbnail(bgra: &[u8], src_w: u32, src_h: u32) -> cairo::ImageSurface {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let sz_i = SWATCH_SIZE as i32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let sz_u = SWATCH_SIZE as u32;
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, sz_i, sz_i)
        .unwrap_or_else(|_| {
            cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).expect("cairo 1x1")
        });
    if src_w == 0 || src_h == 0 || bgra.is_empty() {
        return surface;
    }
    let scale = (f64::from(sz_u) / f64::from(src_w)).min(f64::from(sz_u) / f64::from(src_h));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let dst_w = ((f64::from(src_w) * scale) as u32).max(1).min(sz_u);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let dst_h = ((f64::from(src_h) * scale) as u32).max(1).min(sz_u);
    let x_off = (sz_u - dst_w) / 2;
    let y_off = (sz_u - dst_h) / 2;
    {
        #[allow(clippy::cast_sign_loss)]
        let stride = surface.stride() as usize;
        if let Ok(mut data) = surface.data() {
            data.fill(0);
            for dy in 0..dst_h {
                for dx in 0..dst_w {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let sx = ((f64::from(dx) + 0.5) / scale) as u32;
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let sy = ((f64::from(dy) + 0.5) / scale) as u32;
                    let sx = sx.min(src_w - 1);
                    let sy = sy.min(src_h - 1);
                    let src_i = (sy * src_w + sx) as usize * 4;
                    let dst_i = (dy + y_off) as usize * stride + (dx + x_off) as usize * 4;
                    if src_i + 4 <= bgra.len() && dst_i + 4 <= data.len() {
                        data[dst_i..dst_i + 4].copy_from_slice(&bgra[src_i..src_i + 4]);
                    }
                }
            }
        }
    }
    surface.mark_dirty();
    surface
}
