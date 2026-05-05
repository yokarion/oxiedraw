//! Cairo brush preview for picker rows.
//!
//! Renders a sample S-curve stroke by feeding synthetic `SpawnInput`s through
//! `Dynamics::evaluate` (the same routine the GPU stamp path uses), with a
//! `sin` pressure envelope ramping 0 -> 1 -> 0. Each `Dab` is rasterised with
//! a radial gradient (soft-round / textured) or a hard square (pixel). Colour
//! comes from `gtk::Widget::color()` (CSS `@theme_fg_color`), so previews track
//! the theme like symbolic icons.

use oxiedraw_core::brush_engine::{
    BrushFamily, BrushPreset, Dab, evaluate, make_spawn_input,
};
use oxiedraw_core::color::Color;
use oxiedraw_utils::geometry::Point;
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::prelude::*;

use super::shared;

pub(super) const PREVIEW_WIDTH: i32 = 225;
pub(super) const PREVIEW_HEIGHT: i32 = 64;

pub(super) fn build(preset: &BrushPreset) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(PREVIEW_WIDTH)
        .content_height(PREVIEW_HEIGHT)
        .valign(gtk::Align::Center)
        .build();

    let preset = preset.clone();
    area.set_draw_func(move |area, cr, w, h| {
        let theme = area.color();
        let rgb = (
            f64::from(theme.red()),
            f64::from(theme.green()),
            f64::from(theme.blue()),
        );
        // Prefer the engine-rendered cache if present - it's the
        // ground-truth representation of how the brush actually paints.
        // Fall back to the Cairo synthesis for brushes that don't have
        // a preview cached yet (e.g. mid-backfill on first launch).
        if let Some(bytes) = preset.preview.as_deref()
            && let Some(surface) = shared::decode_preview_png(bytes)
        {
            shared::paint_preview_masked(cr, &surface, f64::from(w), f64::from(h), rgb);
            return;
        }
        draw_preview(cr, w, h, &preset, rgb);
    });
    area
}

fn draw_preview(
    cr: &cairo::Context,
    canvas_w: i32,
    canvas_h: i32,
    preset: &BrushPreset,
    rgb: (f64, f64, f64),
) {
    let canvas_w = f64::from(canvas_w);
    let canvas_h = f64::from(canvas_h);
    let pad_x = 8.0;
    let mid_y = canvas_h * 0.5;
    let amp = canvas_h * 0.22;
    let usable_w = canvas_w - pad_x * 2.0;

    // We render at preview pixel scale (preview_size_px) but the engine
    // operates on the brush's actual default size. Scale converts so a
    // 200 px brush still fits the 32 px row.
    let max_radius = (canvas_h * 0.40).max(1.0);
    #[allow(clippy::cast_possible_truncation)]
    let scale = (max_radius / f64::from(preset.default_size.max(1.0)) * 2.0) as f32;
    let scaled_base_size = preset.default_size * scale;
    let spacing_step =
        f64::from(scaled_base_size) * f64::from(preset.spacing_ratio.max(0.02));
    let spacing_step = spacing_step.max(0.6);

    let path_len = approximate_path_len(usable_w, amp);
    let dab_count = ((path_len / spacing_step).ceil() as usize).clamp(2, 512);

    let is_pixel = matches!(preset.family, BrushFamily::Pixel);
    let path_color = Color::new(0, 0, 0); // unused (we draw with cairo directly)
    let mut cumulative_distance = 0.0_f32;

    for i in 0..dab_count {
        #[allow(clippy::cast_precision_loss)]
        let t = (i as f64) / ((dab_count - 1) as f64);
        let (px, py) = path_at(t, pad_x, usable_w, mid_y, amp);
        let dir = path_tangent_at(t, usable_w, amp);

        // Sine envelope: pressure 0 -> 1 -> 0 across the path.
        #[allow(clippy::cast_possible_truncation)]
        let pressure = (t * std::f64::consts::PI).sin() as f32;

        let mut dab = Dab::round(
            Point::new(px as f32, py as f32),
            scaled_base_size * 0.5,
            path_color,
        );

        if !is_pixel && preset.dynamics.any_active() {
            // Deterministic per-dab random so previews are stable.
            #[allow(clippy::cast_precision_loss)]
            let rand_unit = pseudo_random(preset.id.0, i as u32);
            let scatter_x = pseudo_random(preset.id.0.wrapping_add(0xA5A5), i as u32);
            let scatter_y = pseudo_random(preset.id.0.wrapping_add(0xC3C3), i as u32);
            let input = make_spawn_input(
                pressure,
                /* speed_px_per_ms */ 1.0,
                /* direction */ dir,
                cumulative_distance,
                scaled_base_size,
                rand_unit,
                /* pen_rotation_rad */ 0.0,
                /* tilt_x */ 0.0,
                /* tilt_y */ 0.0,
            );
            evaluate(
                &preset.dynamics,
                &input,
                scaled_base_size,
                (scatter_x, scatter_y),
                &mut dab,
            );
            // Floor for visibility - same MIN_DAB_RADIUS as engine.
            dab.radius = dab.radius.max(0.5);
        } else if !is_pixel {
            // No active dynamics -> modulate radius by pressure for the
            // preview anyway, so the user sees a 0->1->0 sweep rather
            // than a uniform bar.
            dab.radius = (scaled_base_size * 0.5 * pressure).max(0.5);
        }

        draw_dab(cr, &dab, &preset.family, rgb);
        cumulative_distance += spacing_step as f32;
    }
}

fn draw_dab(
    cr: &cairo::Context,
    dab: &Dab,
    family: &BrushFamily,
    rgb: (f64, f64, f64),
) {
    let x = f64::from(dab.center.x);
    let y = f64::from(dab.center.y);
    let r = f64::from(dab.radius);
    let flow = f64::from(dab.flow);
    if r < 0.4 {
        return;
    }

    match family {
        BrushFamily::Pixel => {
            // Hard-edge: no AA, integer-snapped.
            let snapped_x = x.floor() + 0.5;
            let snapped_y = y.floor() + 0.5;
            cr.set_source_rgba(rgb.0, rgb.1, rgb.2, flow);
            cr.rectangle(snapped_x - r, snapped_y - r, r * 2.0, r * 2.0);
            cr.fill().ok();
        }
        BrushFamily::SoftRound => {
            // Radial gradient with a ~1 px feather, matching the
            // `1 - smoothstep(r - aa, r, d)` in dab.frag.
            let gradient = cairo::RadialGradient::new(x, y, 0.0, x, y, r);
            let aa = (r * 0.05).max(0.75);
            let inner_stop = ((r - aa) / r).clamp(0.0, 1.0);
            gradient.add_color_stop_rgba(0.0, rgb.0, rgb.1, rgb.2, flow);
            gradient.add_color_stop_rgba(inner_stop, rgb.0, rgb.1, rgb.2, flow);
            gradient.add_color_stop_rgba(1.0, rgb.0, rgb.1, rgb.2, 0.0);
            cr.set_source(&gradient).ok();
            cr.arc(x, y, r, 0.0, std::f64::consts::TAU);
            cr.fill().ok();
        }
        BrushFamily::Textured(_) => {
            // No atlas sampling on the cairo path; approximate with a
            // softer gradient (longer falloff) to differentiate from
            // soft-round visually.
            let gradient = cairo::RadialGradient::new(x, y, 0.0, x, y, r * 1.2);
            gradient.add_color_stop_rgba(0.0, rgb.0, rgb.1, rgb.2, flow * 0.95);
            gradient.add_color_stop_rgba(0.6, rgb.0, rgb.1, rgb.2, flow * 0.4);
            gradient.add_color_stop_rgba(1.0, rgb.0, rgb.1, rgb.2, 0.0);
            cr.set_source(&gradient).ok();
            cr.arc(x, y, r * 1.2, 0.0, std::f64::consts::TAU);
            cr.fill().ok();
        }
    }
}

fn path_at(t: f64, pad_x: f64, usable_w: f64, mid_y: f64, amp: f64) -> (f64, f64) {
    let x = pad_x + usable_w * t;
    // Half-sine arc: peaks down then up - clean s-curve feel.
    let phase = (t - 0.5) * std::f64::consts::PI * 1.6;
    let y = mid_y + phase.sin() * amp;
    (x, y)
}

#[allow(clippy::cast_possible_truncation)]
fn path_tangent_at(t: f64, usable_w: f64, amp: f64) -> f32 {
    let dt = 0.001;
    let (x0, y0) = path_at((t - dt).max(0.0), 0.0, usable_w, 0.0, amp);
    let (x1, y1) = path_at((t + dt).min(1.0), 0.0, usable_w, 0.0, amp);
    (y1 - y0).atan2(x1 - x0) as f32
}

fn approximate_path_len(usable_w: f64, amp: f64) -> f64 {
    // Discretise the path so spacing math has a real arc length to
    // divide. Coarse sampling is fine - preview is not pixel-perfect.
    let mut len = 0.0;
    let mut prev = path_at(0.0, 0.0, usable_w, 0.0, amp);
    for i in 1..=32 {
        let t = (i as f64) / 32.0;
        let cur = path_at(t, 0.0, usable_w, 0.0, amp);
        let dx = cur.0 - prev.0;
        let dy = cur.1 - prev.1;
        len += dx.hypot(dy);
        prev = cur;
    }
    len
}

#[allow(clippy::cast_precision_loss)]
fn pseudo_random(seed_a: u32, seed_b: u32) -> f32 {
    let mut h = seed_a
        .wrapping_mul(0x9e37_79b9)
        .wrapping_add(seed_b.wrapping_mul(0x6584_3aaa));
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^= h >> 16;
    ((h >> 8) as f32) / ((1u32 << 24) as f32)
}
