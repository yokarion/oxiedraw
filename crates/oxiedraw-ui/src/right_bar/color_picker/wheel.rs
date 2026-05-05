//! Hue ring + HSV triangle drawing and pointer logic.

use std::cell::Cell;
use std::f64::consts::TAU;
use std::rc::Rc;

use oxiedraw_core::color::Color;
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::prelude::*;

use super::{
    HUE_INDICATOR_HALF_ANGLE, PickerState, SV_INDICATOR_RADIUS, TRIANGLE_INSET, WHEEL_INNER_RATIO,
    WHEEL_OUTER_RATIO, WHEEL_SIZE,
};

pub(super) fn build_wheel_widget() -> gtk::DrawingArea {
    gtk::DrawingArea::builder()
        .content_width(WHEEL_SIZE)
        .content_height(WHEEL_SIZE)
        .halign(gtk::Align::Center)
        .build()
}

pub(super) fn install_wheel_draw(area: &gtk::DrawingArea, state: &PickerState) {
    let state = state.clone();
    area.set_draw_func(move |_, ctx, w, h| {
        let geom = WheelGeom::for_area(w, h);
        draw_hue_ring(ctx, &geom);
        draw_hue_indicator(ctx, &geom, f64::from(state.hue.get()));
        draw_triangle(ctx, &geom, f64::from(state.hue.get()));
        draw_sv_indicator(
            ctx,
            &geom,
            f64::from(state.hue.get()),
            f64::from(state.saturation.get()),
            f64::from(state.value.get()),
        );
    });
}

pub(super) fn install_wheel_input(
    area: &gtk::DrawingArea,
    state: &PickerState,
    refresh: &Rc<dyn Fn()>,
) {
    let drag = gtk::GestureDrag::new();
    drag.set_button(gtk::gdk::BUTTON_PRIMARY);

    let mode: Rc<Cell<DragMode>> = Rc::new(Cell::new(DragMode::None));

    {
        let area_w = area.clone();
        let state = state.clone();
        let mode = Rc::clone(&mode);
        let refresh = Rc::clone(refresh);
        drag.connect_drag_begin(move |_, x, y| {
            let geom = WheelGeom::for_area(area_w.width(), area_w.height());
            let new_mode = classify_pointer(&geom, x, y, f64::from(state.hue.get()));
            mode.set(new_mode);
            apply_pointer(&state, &geom, x, y, new_mode);
            refresh();
        });
    }
    {
        let area_w = area.clone();
        let state = state.clone();
        let mode = Rc::clone(&mode);
        let refresh = Rc::clone(refresh);
        drag.connect_drag_update(move |gesture, dx, dy| {
            let Some((sx, sy)) = gesture.start_point() else {
                return;
            };
            let geom = WheelGeom::for_area(area_w.width(), area_w.height());
            apply_pointer(&state, &geom, sx + dx, sy + dy, mode.get());
            refresh();
        });
    }
    {
        let mode = Rc::clone(&mode);
        drag.connect_drag_end(move |_, _, _| mode.set(DragMode::None));
    }
    area.add_controller(drag);
}

#[derive(Clone, Copy, Debug)]
enum DragMode {
    None,
    Hue,
    Triangle,
}

fn classify_pointer(geom: &WheelGeom, x: f64, y: f64, hue: f64) -> DragMode {
    let dx = x - geom.cx;
    let dy = y - geom.cy;
    let r = dx.hypot(dy);
    if r >= geom.inner_r * 0.98 && r <= geom.outer_r * 1.02 {
        return DragMode::Hue;
    }
    if point_in_triangle(geom, hue, x, y) {
        return DragMode::Triangle;
    }
    // fall back to whichever is closer
    if r > (geom.inner_r + geom.outer_r) * 0.5 {
        DragMode::Hue
    } else {
        DragMode::Triangle
    }
}

fn apply_pointer(state: &PickerState, geom: &WheelGeom, x: f64, y: f64, mode: DragMode) {
    match mode {
        DragMode::Hue => {
            let dx = x - geom.cx;
            let dy = y - geom.cy;
            let angle = dy.atan2(dx);
            #[allow(clippy::cast_possible_truncation)]
            let h = (angle.rem_euclid(TAU) / TAU) as f32;
            state.hue.set(h);
            state.write_color_from_hsv();
        }
        DragMode::Triangle => {
            let (s, v) = sv_from_point(geom, f64::from(state.hue.get()), x, y);
            #[allow(clippy::cast_possible_truncation)]
            {
                state.saturation.set(s as f32);
                state.value.set(v as f32);
            }
            state.write_color_from_hsv();
        }
        DragMode::None => {}
    }
}

#[derive(Clone, Copy)]
struct WheelGeom {
    cx: f64,
    cy: f64,
    outer_r: f64,
    inner_r: f64,
    triangle_r: f64,
}

impl WheelGeom {
    fn for_area(w: i32, h: i32) -> Self {
        let cx = f64::from(w) / 2.0;
        let cy = f64::from(h) / 2.0;
        let size = f64::from(w.min(h));
        let outer_r = size * WHEEL_OUTER_RATIO;
        let inner_r = size * WHEEL_INNER_RATIO;
        let triangle_r = inner_r * TRIANGLE_INSET;
        Self {
            cx,
            cy,
            outer_r,
            inner_r,
            triangle_r,
        }
    }
}

fn draw_hue_ring(ctx: &cairo::Context, geom: &WheelGeom) {
    const SEGMENTS: u32 = 360;
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    for i in 0..SEGMENTS {
        let h = i as f32 / SEGMENTS as f32;
        let c = Color::from_hsv(h, 1.0, 1.0);
        let a0 = f64::from(i) / f64::from(SEGMENTS) * TAU;
        let a1 = f64::from(i + 1) / f64::from(SEGMENTS) * TAU;
        ctx.set_source_rgb(
            f64::from(c.r) / 255.0,
            f64::from(c.g) / 255.0,
            f64::from(c.b) / 255.0,
        );
        ctx.move_to(geom.cx, geom.cy);
        ctx.arc(geom.cx, geom.cy, geom.outer_r, a0, a1 + 0.005);
        ctx.line_to(geom.cx, geom.cy);
        ctx.fill().ok();
    }
    // punch out the inner disk
    ctx.set_operator(cairo::Operator::Clear);
    ctx.arc(geom.cx, geom.cy, geom.inner_r, 0.0, TAU);
    ctx.fill().ok();
    ctx.set_operator(cairo::Operator::Over);

    // outer + inner stroke
    ctx.set_source_rgba(0.0, 0.0, 0.0, 0.45);
    ctx.set_line_width(1.0);
    ctx.arc(geom.cx, geom.cy, geom.outer_r, 0.0, TAU);
    ctx.stroke().ok();
    ctx.arc(geom.cx, geom.cy, geom.inner_r, 0.0, TAU);
    ctx.stroke().ok();
}

fn draw_hue_indicator(ctx: &cairo::Context, geom: &WheelGeom, hue: f64) {
    let angle = hue * TAU;
    let a0 = angle - HUE_INDICATOR_HALF_ANGLE;
    let a1 = angle + HUE_INDICATOR_HALF_ANGLE;
    let pad = 1.5;

    ctx.set_source_rgb(1.0, 1.0, 1.0);
    ctx.set_line_width(2.0);
    ctx.arc(geom.cx, geom.cy, geom.outer_r + pad, a0, a1);
    ctx.arc_negative(geom.cx, geom.cy, geom.inner_r - pad, a1, a0);
    ctx.close_path();
    ctx.stroke_preserve().ok();
    ctx.set_source_rgb(0.0, 0.0, 0.0);
    ctx.set_line_width(1.0);
    ctx.stroke().ok();
}

fn triangle_vertices(geom: &WheelGeom, hue: f64) -> [(f64, f64); 3] {
    // The hue vertex sits at the angle on the ring matching the current hue;
    // the other two vertices follow at 120deg offsets.
    let base = hue * TAU;
    let angles = [base, base + TAU / 3.0, base + 2.0 * TAU / 3.0];
    let vertex = |a: f64| {
        (
            geom.triangle_r.mul_add(a.cos(), geom.cx),
            geom.triangle_r.mul_add(a.sin(), geom.cy),
        )
    };
    [vertex(angles[0]), vertex(angles[1]), vertex(angles[2])]
}

fn draw_triangle(ctx: &cairo::Context, geom: &WheelGeom, hue: f64) {
    #[allow(clippy::cast_possible_truncation)]
    let hue_color = Color::from_hsv(hue as f32, 1.0, 1.0);
    let v = triangle_vertices(geom, hue);

    let mesh = cairo::Mesh::new();
    mesh.begin_patch();
    mesh.move_to(v[0].0, v[0].1);
    mesh.line_to(v[1].0, v[1].1);
    mesh.line_to(v[2].0, v[2].1);
    mesh.line_to(v[0].0, v[0].1);
    mesh.set_corner_color_rgb(
        cairo::MeshCorner::MeshCorner0,
        f64::from(hue_color.r) / 255.0,
        f64::from(hue_color.g) / 255.0,
        f64::from(hue_color.b) / 255.0,
    );
    mesh.set_corner_color_rgb(cairo::MeshCorner::MeshCorner1, 1.0, 1.0, 1.0);
    mesh.set_corner_color_rgb(cairo::MeshCorner::MeshCorner2, 0.0, 0.0, 0.0);
    mesh.set_corner_color_rgb(
        cairo::MeshCorner::MeshCorner3,
        f64::from(hue_color.r) / 255.0,
        f64::from(hue_color.g) / 255.0,
        f64::from(hue_color.b) / 255.0,
    );
    mesh.end_patch();

    ctx.move_to(v[0].0, v[0].1);
    ctx.line_to(v[1].0, v[1].1);
    ctx.line_to(v[2].0, v[2].1);
    ctx.close_path();
    ctx.set_source(&mesh).ok();
    ctx.fill_preserve().ok();
    ctx.set_source_rgba(0.0, 0.0, 0.0, 0.6);
    ctx.set_line_width(1.0);
    ctx.stroke().ok();
}

fn sv_to_point(geom: &WheelGeom, hue: f64, sat: f64, val: f64) -> (f64, f64) {
    let verts = triangle_vertices(geom, hue);
    let weight_hue = sat * val;
    let weight_white = val * (1.0 - sat);
    let weight_black = 1.0 - val;
    (
        weight_hue.mul_add(
            verts[0].0,
            weight_black.mul_add(verts[2].0, weight_white * verts[1].0),
        ),
        weight_hue.mul_add(
            verts[0].1,
            weight_black.mul_add(verts[2].1, weight_white * verts[1].1),
        ),
    )
}

fn sv_from_point(geom: &WheelGeom, hue: f64, px: f64, py: f64) -> (f64, f64) {
    let verts = triangle_vertices(geom, hue);
    let (wh, ww, wb) = barycentric(verts, px, py);
    let (wh, ww, _) = clamp_barycentric(wh, ww, wb);
    let value = (wh + ww).clamp(0.0, 1.0);
    let saturation = if wh + ww > f64::EPSILON {
        (wh / (wh + ww)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (saturation, value)
}

#[allow(clippy::many_single_char_names)]
fn barycentric(v: [(f64, f64); 3], px: f64, py: f64) -> (f64, f64, f64) {
    let (x1, y1) = v[0];
    let (x2, y2) = v[1];
    let (x3, y3) = v[2];
    let denom = (y2 - y3).mul_add(x1 - x3, (x3 - x2) * (y1 - y3));
    if denom.abs() < f64::EPSILON {
        return (1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0);
    }
    let a = ((y2 - y3).mul_add(px - x3, (x3 - x2) * (py - y3))) / denom;
    let b = ((y3 - y1).mul_add(px - x3, (x1 - x3) * (py - y3))) / denom;
    let c = 1.0 - a - b;
    (a, b, c)
}

fn clamp_barycentric(a: f64, b: f64, c: f64) -> (f64, f64, f64) {
    let a = a.max(0.0);
    let b = b.max(0.0);
    let c = c.max(0.0);
    let sum = a + b + c;
    if sum <= f64::EPSILON {
        (1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0)
    } else {
        (a / sum, b / sum, c / sum)
    }
}

fn point_in_triangle(geom: &WheelGeom, hue: f64, px: f64, py: f64) -> bool {
    let verts = triangle_vertices(geom, hue);
    let (a, b, c) = barycentric(verts, px, py);
    a >= 0.0 && b >= 0.0 && c >= 0.0
}

fn draw_sv_indicator(ctx: &cairo::Context, geom: &WheelGeom, hue: f64, s: f64, v: f64) {
    let (px, py) = sv_to_point(geom, hue, s, v);
    ctx.set_line_width(1.5);
    ctx.set_source_rgb(1.0, 1.0, 1.0);
    ctx.arc(px, py, SV_INDICATOR_RADIUS, 0.0, TAU);
    ctx.stroke_preserve().ok();
    ctx.set_source_rgb(0.0, 0.0, 0.0);
    ctx.set_line_width(0.8);
    ctx.arc(px, py, SV_INDICATOR_RADIUS + 1.0, 0.0, TAU);
    ctx.stroke().ok();
}

// ---------------------------------------------------------------------------
// Primary / secondary swatch
// ---------------------------------------------------------------------------
