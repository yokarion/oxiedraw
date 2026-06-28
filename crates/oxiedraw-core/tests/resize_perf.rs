//! Timed reproduction of the "stylus drawing is laggy only after an in-session
//! canvas resize" bug. Drives the real `Canvas` draw path (stamp + present, the
//! per-motion-event hot loop) and measures throughput before vs after an
//! `apply_crop` resize. GPU-gated; run with:
//!
//!   cargo test -p oxiedraw-core --test resize_perf -- --ignored --nocapture
//!
//! These print per-iteration timings so the regression is visible, and assert
//! that the post-resize draw loop is not dramatically slower than before.

#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use oxiedraw_core::brush_engine::{BrushEngine, InputSample};
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::Color;
use oxiedraw_core::tools::CropRect;
use oxiedraw_utils::geometry::Point;

const RED: Color = Color::new(255, 0, 0);

fn sample(x: f32, y: f32, t: u64) -> InputSample {
    InputSample {
        position: Point::new(x, y),
        pressure: 1.0,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rotation: 0.0,
        time_ms: t,
    }
}

/// Run a realistic drawing burst: one `stamp` (a dab) + one `present` per
/// iteration, exactly what a stylus motion event triggers. Returns total time
/// and the slowest single iteration (the jitter spike).
fn draw_loop(canvas: &mut Canvas, brush: &BrushEngine, w: u32, iters: u32) -> (Duration, Duration) {
    canvas.begin_stroke(RED, 1.0, false).unwrap();
    let first = sample(8.0, 8.0, 0);
    canvas.stamp(|t| brush.begin_stroke(first, RED, t)).unwrap();

    let mut total = Duration::ZERO;
    let mut worst = Duration::ZERO;
    for i in 0..iters {
        // Walk a diagonal across the canvas so dabs spread over the surface.
        let f = i as f32 / iters as f32;
        let x = 8.0 + f * (w as f32 - 16.0);
        let y = 8.0 + f * 200.0;
        let s = sample(x, y, u64::from(i + 1) * 8);

        let t0 = Instant::now();
        canvas.stamp(|tgt| brush.push_sample(s, tgt)).unwrap();
        let _desc = canvas.present().unwrap();
        let dt = t0.elapsed();

        total += dt;
        worst = worst.max(dt);
    }
    canvas.commit_stroke().unwrap();
    (total, worst)
}

fn new_brush() -> BrushEngine {
    let brush = BrushEngine::new();
    brush.size.set(24.0);
    brush.opacity.set(1.0);
    brush
}

/// Core reproduction: the same draw loop, before and after a resize, on one
/// `Canvas`. If the bug lives in the core present/composite path, the
/// post-resize loop is measurably slower here.
#[test]
#[ignore = "requires vulkan loader and device"]
fn draw_loop_before_vs_after_resize() {
    let brush = new_brush();
    let mut canvas = Canvas::headless(oxiedraw_utils::geometry::Size::new(2048, 2048)).unwrap();
    let iters = 240;

    // Warm up so first-frame allocation costs don't skew the "before" number.
    let _ = draw_loop(&mut canvas, &brush, 2048, 40);

    let (before_total, before_worst) = draw_loop(&mut canvas, &brush, 2048, iters);

    // Expand the width 2048 -> 3072, the exact user action.
    let new_size = canvas.apply_crop(CropRect::new(0.0, 0.0, 3072.0, 2048.0)).unwrap();
    assert_eq!(new_size.width, 3072);

    let _ = draw_loop(&mut canvas, &brush, 3072, 40);
    let (after_total, after_worst) = draw_loop(&mut canvas, &brush, 3072, iters);

    let before_avg = before_total.as_secs_f64() * 1000.0 / f64::from(iters);
    let after_avg = after_total.as_secs_f64() * 1000.0 / f64::from(iters);
    eprintln!("--- draw loop (stamp+present) per iteration ---");
    eprintln!("BEFORE resize (2048): avg {before_avg:.3} ms, worst {:.3} ms", before_worst.as_secs_f64() * 1000.0);
    eprintln!("AFTER  resize (3072): avg {after_avg:.3} ms, worst {:.3} ms", after_worst.as_secs_f64() * 1000.0);
    eprintln!("ratio after/before: {:.2}x", after_avg / before_avg);

    // A 3072-wide canvas is 1.5x the pixels of 2048-wide, so some increase is
    // expected. A genuine regression shows up as a multiple far beyond that.
    assert!(
        after_avg < before_avg * 3.0,
        "post-resize draw loop is {:.2}x slower - regression",
        after_avg / before_avg,
    );
}

/// Control: a canvas created fresh at 3072 (no resize) vs one resized to 3072.
/// Same final dimensions; if only the *resized* one is slow, the resize itself
/// (not the size) is the culprit. This is the precise shape of the bug report.
#[test]
#[ignore = "requires vulkan loader and device"]
fn resized_vs_fresh_same_size() {
    let brush = new_brush();
    let iters = 240;

    // Fresh 3072-wide canvas.
    let mut fresh = Canvas::headless(oxiedraw_utils::geometry::Size::new(3072, 2048)).unwrap();
    let _ = draw_loop(&mut fresh, &brush, 3072, 40);
    let (fresh_total, fresh_worst) = draw_loop(&mut fresh, &brush, 3072, iters);

    // Canvas resized from 2048 to 3072.
    let mut resized = Canvas::headless(oxiedraw_utils::geometry::Size::new(2048, 2048)).unwrap();
    let _ = draw_loop(&mut resized, &brush, 2048, 40);
    let _ = resized.apply_crop(CropRect::new(0.0, 0.0, 3072.0, 2048.0)).unwrap();
    let _ = draw_loop(&mut resized, &brush, 3072, 40);
    let (resized_total, resized_worst) = draw_loop(&mut resized, &brush, 3072, iters);

    let fresh_avg = fresh_total.as_secs_f64() * 1000.0 / f64::from(iters);
    let resized_avg = resized_total.as_secs_f64() * 1000.0 / f64::from(iters);
    eprintln!("--- fresh 3072 vs resized-to-3072 (stamp+present) ---");
    eprintln!("FRESH   3072: avg {fresh_avg:.3} ms, worst {:.3} ms", fresh_worst.as_secs_f64() * 1000.0);
    eprintln!("RESIZED 3072: avg {resized_avg:.3} ms, worst {:.3} ms", resized_worst.as_secs_f64() * 1000.0);
    eprintln!("ratio resized/fresh: {:.2}x", resized_avg / fresh_avg);

    assert!(
        resized_avg < fresh_avg * 2.0,
        "resized canvas draws {:.2}x slower than an identical fresh one - resize regression",
        resized_avg / fresh_avg,
    );
}
