//! Large stroke preview shown at the bottom of the Manage Brushes
//! window. Renders the *actual* engine output by driving a headless
//! `Canvas` (via [`oxiedraw_core::brush_engine::preview_renderer`]) so
//! what the user sees is what the brush actually paints - no Cairo
//! approximation in this surface.
//!
//! A short debounce coalesces the burst of `set_brush` calls that
//! every slider-drag produces into one engine render. The same PNG
//! bytes are later cached on the preset and written into the
//! `.oxiebrush` archive by the save path; this widget owns the live
//! copy that drives the visible surface between saves.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use oxiedraw_core::brush_engine::{BrushPreset, preview_renderer};
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::brush_picker::shared as picker_shared;

const PREVIEW_HEIGHT: i32 = 140;
const CHECKER_SIZE: f64 = 12.0;
/// Time we wait after the last `set_brush` before issuing an engine
/// render. Short enough that the user feels the response, long enough
/// that a slider drag emits 50 events and only renders once.
const LIVE_DEBOUNCE: Duration = Duration::from_millis(80);

/// Build a fixed-height DrawingArea backed by an engine-rendered
/// preview. Returns the area plus a setter; calling the setter with
/// `Some(preset)` schedules a live render, `None` clears the surface.
pub(super) fn build() -> (gtk::DrawingArea, Rc<dyn Fn(Option<&BrushPreset>)>) {
    let area = gtk::DrawingArea::builder()
        .content_height(PREVIEW_HEIGHT)
        .hexpand(true)
        .vexpand(false)
        .build();

    // `current_preset` holds the latest brush state passed to setter;
    // `live_png` holds the most recent engine render. They're updated
    // on different cadences: the preset moves in lockstep with user
    // edits, the PNG is regenerated through a debounce.
    let current_preset: Rc<RefCell<Option<BrushPreset>>> = Rc::new(RefCell::new(None));
    let live_png: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
    let pending_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    {
        let area = area.clone();
        let current_preset = current_preset.clone();
        let live_png = live_png.clone();
        area.set_draw_func(move |area, cr, w, h| {
            let theme = area.color();
            let fg = (
                f64::from(theme.red()),
                f64::from(theme.green()),
                f64::from(theme.blue()),
            );
            draw_checker(cr, w, h);
            // If we have a live engine render, paint that. Otherwise
            // fall back to the cached preview on the preset (e.g. the
            // moment the editor opens, before the first debounce
            // fires) so the user never sees an empty checker.
            let png_ref = live_png.borrow();
            let preset_ref = current_preset.borrow();
            let bytes: Option<&[u8]> = png_ref
                .as_deref()
                .or_else(|| preset_ref.as_ref().and_then(|p| p.preview.as_deref()));
            if let Some(bytes) = bytes
                && let Some(surface) = picker_shared::decode_preview_png(bytes)
            {
                picker_shared::paint_preview_masked(
                    cr,
                    &surface,
                    f64::from(w),
                    f64::from(h),
                    fg,
                );
            }
        });
    }

    let setter: Rc<dyn Fn(Option<&BrushPreset>)> = {
        let area = area.clone();
        let current_preset = current_preset.clone();
        let live_png = live_png.clone();
        let pending_timer = pending_timer.clone();
        Rc::new(move |maybe: Option<&BrushPreset>| {
            *current_preset.borrow_mut() = maybe.cloned();
            if maybe.is_none() {
                *live_png.borrow_mut() = None;
                if let Some(timer) = pending_timer.borrow_mut().take() {
                    timer.remove();
                }
                area.queue_draw();
                return;
            }
            // Repaint immediately with whatever bytes we already have
            // (cached preview on the preset, if any) so the user sees
            // *something* during the debounce window.
            area.queue_draw();
            schedule_live_render(
                &current_preset,
                &live_png,
                &pending_timer,
                &area,
            );
        })
    };

    (area, setter)
}

/// Debounce -> engine render -> store PNG -> redraw. Replaces any
/// pending timer so a fresh edit pushes the render window back; the
/// most recent preset state is what gets rendered when the timer
/// finally fires.
fn schedule_live_render(
    current_preset: &Rc<RefCell<Option<BrushPreset>>>,
    live_png: &Rc<RefCell<Option<Vec<u8>>>>,
    pending_timer: &Rc<RefCell<Option<glib::SourceId>>>,
    area: &gtk::DrawingArea,
) {
    if let Some(prev) = pending_timer.borrow_mut().take() {
        prev.remove();
    }
    let current_preset = current_preset.clone();
    let live_png = live_png.clone();
    let pending_timer_inner = pending_timer.clone();
    let area = area.clone();
    let source_id = glib::timeout_add_local_once(LIVE_DEBOUNCE, move || {
        pending_timer_inner.borrow_mut().take();
        // Clone out of the Rc<RefCell<...>> so the borrow is dropped
        // before the (potentially expensive) Vulkan render runs.
        let Some(preset) = current_preset.borrow().clone() else { return };
        match preview_renderer::render_preview_png(&preset) {
            Ok(png) => {
                *live_png.borrow_mut() = Some(png);
            }
            Err(e) => {
                tracing::warn!(brush = %preset.name, %e, "live preview render failed");
                // Keep whatever we had - better than blanking on a
                // transient driver hiccup.
            }
        }
        area.queue_draw();
    });
    *pending_timer.borrow_mut() = Some(source_id);
}

fn draw_checker(cr: &cairo::Context, w: i32, h: i32) {
    let w = f64::from(w);
    let h = f64::from(h);
    cr.set_source_rgb(0.78, 0.78, 0.78);
    cr.rectangle(0.0, 0.0, w, h);
    cr.fill().ok();
    cr.set_source_rgb(0.60, 0.60, 0.60);
    let mut y = 0.0;
    let mut row = 0;
    while y < h {
        let mut x = if row % 2 == 0 { CHECKER_SIZE } else { 0.0 };
        while x < w {
            cr.rectangle(x, y, CHECKER_SIZE, CHECKER_SIZE);
            x += CHECKER_SIZE * 2.0;
        }
        cr.fill().ok();
        y += CHECKER_SIZE;
        row += 1;
    }
}
