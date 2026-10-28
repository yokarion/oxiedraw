//! GPU integration tests for adjustment layers, driven through the public
//! `Canvas` API.
//!
//! Adjustment layers filter the composited backdrop (everything below them), so
//! these read the composited canvas via `read_pixels`, not a single layer.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::Color;
use oxiedraw_core::curves::{Curve, CurveChannel, CurvePoint, CurveSet};
use oxiedraw_core::effects::{AdjustmentData, Effect, EffectKind, StrokeSoftness};
use oxiedraw_core::filters::BlurKind;
use oxiedraw_utils::geometry::Size;

/// Fill a full-canvas BGRA8 buffer with one opaque color (B, G, R).
fn solid(size: Size, b: u8, g: u8, r: u8) -> Vec<u8> {
    let n = (size.width * size.height) as usize;
    let mut px = vec![0u8; n * 4];
    for chunk in px.chunks_exact_mut(4) {
        chunk.copy_from_slice(&[b, g, r, 255]);
    }
    px
}

fn near(a: u8, b: u8, tol: i32) -> bool {
    (i32::from(a) - i32::from(b)).abs() <= tol
}

fn one_effect(kind: EffectKind) -> AdjustmentData {
    AdjustmentData {
        effects: vec![Effect::new(kind)],
    }
}

/// The incremental adjusted preview (local Hue/Sat/Bright effect) must match a
/// full rebuild while painting a layer below the adjustment.
#[test]
fn incremental_adjusted_preview_matches_full() {
    use oxiedraw_core::brush_engine::{BrushEngine, InputSample};
    use oxiedraw_core::color::Color;
    use oxiedraw_utils::geometry::Point;

    let sample = |x: f32, y: f32, t: u64| InputSample {
        position: Point::new(x, y),
        pressure: 1.0,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rotation: 0.0,
        time_ms: t,
    };

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let base = canvas
        .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.5, // local, non-identity
            }),
        )
        .unwrap();
    canvas.layers().set_active(Some(base));

    let brush = BrushEngine::new();
    brush.size.set(6.0);
    brush.opacity.set(1.0);
    let white = Color::new(255, 255, 255);
    canvas.begin_stroke(white, 1.0, false).unwrap();

    canvas
        .stamp(|t| brush.begin_stroke(sample(12.0, 12.0, 0), white, t))
        .unwrap();
    canvas
        .stamp(|t| brush.push_sample(sample(16.0, 12.0, 10), t))
        .unwrap();
    let _ = canvas.read_incremental_preview().unwrap(); // frame 1 (full)

    canvas
        .stamp(|t| brush.push_sample(sample(48.0, 50.0, 20), t))
        .unwrap();
    canvas
        .stamp(|t| brush.push_sample(sample(52.0, 52.0, 30), t))
        .unwrap();
    let incremental = canvas.read_incremental_preview().unwrap(); // frame 2 (incremental)

    canvas.force_full_preview();
    let full = canvas.read_incremental_preview().unwrap();

    assert_eq!(incremental.len(), full.len());
    let diff = incremental
        .iter()
        .zip(full.iter())
        .filter(|(a, b)| (i32::from(**a) - i32::from(**b)).abs() > 2)
        .count();
    assert_eq!(diff, 0, "incremental adjusted diverged from full in {diff} bytes");
}

/// The incremental adjusted preview with a *non-local* Stroke effect must match
/// a full rebuild: the two-region (inner output / outer input) clip keeps the
/// stroke band correct as the silhouette is painted.
#[test]
fn incremental_stroke_preview_matches_full() {
    use oxiedraw_core::brush_engine::{BrushEngine, InputSample};
    use oxiedraw_core::color::Color;
    use oxiedraw_core::effects::StrokeSoftness;
    use oxiedraw_utils::geometry::Point;

    let sample = |x: f32, y: f32, t: u64| InputSample {
        position: Point::new(x, y),
        pressure: 1.0,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rotation: 0.0,
        time_ms: t,
    };

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    // Transparent base: the white strokes themselves form the silhouette the
    // stroke effect traces.
    let base = canvas
        .add_layer_with_pixels("base", &vec![0u8; (size.width * size.height) as usize * 4])
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::Stroke {
                color: Color { r: 255, g: 0, b: 0 },
                opacity: 1.0,
                thickness: 5.0,
                offset: 1.0,
                softness: StrokeSoftness::Pixelated,
            }),
        )
        .unwrap();
    canvas.layers().set_active(Some(base));

    let brush = BrushEngine::new();
    brush.size.set(8.0);
    brush.opacity.set(1.0);
    let white = Color::new(255, 255, 255);
    canvas.begin_stroke(white, 1.0, false).unwrap();

    canvas
        .stamp(|t| brush.begin_stroke(sample(16.0, 16.0, 0), white, t))
        .unwrap();
    canvas
        .stamp(|t| brush.push_sample(sample(22.0, 18.0, 10), t))
        .unwrap();
    let _ = canvas.read_incremental_preview().unwrap();

    canvas
        .stamp(|t| brush.push_sample(sample(40.0, 44.0, 20), t))
        .unwrap();
    canvas
        .stamp(|t| brush.push_sample(sample(46.0, 48.0, 30), t))
        .unwrap();
    let incremental = canvas.read_incremental_preview().unwrap();

    canvas.force_full_preview();
    let full = canvas.read_incremental_preview().unwrap();

    assert_eq!(incremental.len(), full.len());
    let diff = incremental
        .iter()
        .zip(full.iter())
        .filter(|(a, b)| (i32::from(**a) - i32::from(**b)).abs() > 2)
        .count();
    assert_eq!(diff, 0, "incremental stroke diverged from full in {diff} bytes");
}

fn line_sample(x: f32, y: f32, t: u64) -> oxiedraw_core::brush_engine::InputSample {
    oxiedraw_core::brush_engine::InputSample {
        position: oxiedraw_utils::geometry::Point::new(x, y),
        pressure: 1.0,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rotation: 0.0,
        time_ms: t,
    }
}

// Drives the live path (stamp + present) along a line at y = 48 and reads the
// display back twice: after an incremental second frame, and after a forced
// full frame. The display keeps every pixel the incremental frame did not
// present, so the two must agree everywhere.
fn incremental_and_full_line(canvas: &mut Canvas, target: usize, color: Color) -> (Vec<u8>, Vec<u8>) {
    use oxiedraw_core::brush_engine::BrushEngine;

    canvas.layers().set_active(Some(target));
    let brush = BrushEngine::new();
    brush.size.set(6.0);
    brush.opacity.set(1.0);
    canvas.begin_stroke(color, 1.0, false).unwrap();
    canvas.stamp(|t| brush.begin_stroke(line_sample(10.0, 48.0, 0), color, t)).unwrap();
    canvas.stamp(|t| brush.push_sample(line_sample(14.0, 48.0, 10), t)).unwrap();
    canvas.present().unwrap();

    canvas.stamp(|t| brush.push_sample(line_sample(50.0, 48.0, 20), t)).unwrap();
    canvas.stamp(|t| brush.push_sample(line_sample(80.0, 48.0, 30), t)).unwrap();
    canvas.present().unwrap();
    let incremental = canvas.read_display().unwrap();
    canvas.force_full_preview();
    canvas.present().unwrap();
    (incremental, canvas.read_display().unwrap())
}

// Stripes on the top half over a transparent bottom half: an alpha edge along
// y = 48 for the stroke effect to trace, under the line the helpers paint.
fn half_stripes(size: Size) -> Vec<u8> {
    (0..size.height)
        .flat_map(|y| {
            let v = if y / 2 % 2 == 0 { 100 } else { 150 };
            let px = if y < size.height / 2 { [v, v, v, 255] } else { [0; 4] };
            px.repeat(size.width as usize)
        })
        .collect()
}

fn every_effect() -> Vec<EffectKind> {
    vec![
        EffectKind::HueSatBright {
            hue_degrees: 20.0,
            saturation: 1.2,
            brightness: 0.9,
        },
        EffectKind::Curves {
            curves: CurveSet::default(),
        },
        EffectKind::Blur {
            kind: BlurKind::Gaussian,
            radius_x: 3.0,
            radius_y: 3.0,
        },
        EffectKind::sharpen_default(),
        EffectKind::Stroke {
            color: Color { r: 255, g: 0, b: 0 },
            opacity: 1.0,
            thickness: 6.0,
            offset: 1.0,
            softness: StrokeSoftness::Bilinear,
        },
    ]
}

fn count_differing(a: &[u8], b: &[u8], tolerance: i32) -> usize {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).filter(|(x, y)| !near(**x, **y, tolerance)).count()
}

/// Bounding box `(x0, y0, x1, y1)` of the pixels where `a` and `b` differ.
fn diff_bbox(a: &[u8], b: &[u8], width: u32, tolerance: i32) -> Option<(u32, u32, u32, u32)> {
    let mut bbox: Option<(u32, u32, u32, u32)> = None;
    for (i, (pa, pb)) in a.chunks_exact(4).zip(b.chunks_exact(4)).enumerate() {
        if pa.iter().zip(pb).all(|(x, y)| near(*x, *y, tolerance)) {
            continue;
        }
        let (x, y) = (i as u32 % width, i as u32 / width);
        bbox = Some(bbox.map_or((x, y, x, y), |(x0, y0, x1, y1)| {
            (x0.min(x), y0.min(y), x1.max(x), y1.max(y))
        }));
    }
    bbox
}

/// Whole-image check of the incremental preview under a blur on content that
/// varies, so a pass reading past the seeded region shows up.
#[test]
fn incremental_blur_preview_matches_full_on_stripes() {
    let size = Size::new(256, 96);
    let cases = [
        EffectKind::Blur {
            kind: BlurKind::Gaussian,
            radius_x: 4.0,
            radius_y: 4.0,
        },
        EffectKind::Blur {
            kind: BlurKind::Box,
            radius_x: 6.0,
            radius_y: 2.0,
        },
        EffectKind::sharpen_default(),
    ];
    for effect in cases {
        let mut canvas = Canvas::headless(size).unwrap();
        let base = canvas.add_layer_with_pixels("base", &half_stripes(size)).unwrap();
        adjust_with(&mut canvas, &[effect]);
        let (incremental, full) = incremental_and_full_line(&mut canvas, base, Color::WHITE);
        let bbox = diff_bbox(&incremental, &full, size.width, 2);
        assert_eq!(count_differing(&incremental, &full, 2), 0, "{effect:?} differs within {bbox:?}");
    }
}

/// Painting an adjustment's mask (mask view off) previews the effect through
/// the incremental frame; it must match both a full frame and the scoped walk
/// folder-bounded documents still use.
#[test]
fn incremental_mask_paint_preview_matches_full() {
    let size = Size::new(256, 96);
    let backdrop = half_stripes(size);
    let cases = [
        vec![EffectKind::HueSatBright {
            hue_degrees: 0.0,
            saturation: 1.0,
            brightness: 0.4,
        }],
        vec![EffectKind::Blur {
            kind: BlurKind::Gaussian,
            radius_x: 4.0,
            radius_y: 4.0,
        }],
        vec![EffectKind::Stroke {
            color: Color { r: 0, g: 0, b: 255 },
            opacity: 1.0,
            thickness: 8.0,
            offset: 0.0,
            softness: StrokeSoftness::Pixelated,
        }],
        every_effect(),
    ];
    for effects in cases {
        let mut canvas = Canvas::headless(size).unwrap();
        canvas.add_layer_with_pixels("base", &backdrop).unwrap();
        let adj = canvas.add_adjustment_layer("adj").unwrap();
        let data = AdjustmentData {
            effects: effects.iter().copied().map(Effect::new).collect(),
        };
        canvas.set_layer_effects(adj, data).unwrap();
        // Black on the white mask switches the effect off under the line.
        let (incremental, full) = incremental_and_full_line(&mut canvas, adj, Color::BLACK);
        let bbox = diff_bbox(&incremental, &full, size.width, 2);
        assert_eq!(count_differing(&incremental, &full, 2), 0, "{effects:?} differs within {bbox:?}");

        // The same full frame, read in linear space from the batched path and
        // from the scoped walk that folder-bounded documents still take.
        canvas.force_full_preview();
        let batched = canvas.read_incremental_preview().unwrap();
        let walked = canvas.read_pixels().unwrap();
        assert_eq!(count_differing(&batched, &walked, 2), 0, "{effects:?} vs scoped walk");
        // The stroke changed something, or the comparison proves nothing.
        canvas.discard_stroke().unwrap();
        let untouched = canvas.read_pixels().unwrap();
        assert_ne!(count_differing(&batched, &untouched, 2), 0, "{effects:?} had no effect");
    }
}

/// Every stroke commit patches the canvas in place (the dirty rect grown by the
/// effects above the layer); the result must equal a full rebuild.
#[test]
fn incremental_commit_matches_full_rebuild() {
    use oxiedraw_core::brush_engine::BrushEngine;

    let size = Size::new(256, 96);
    let backdrop = half_stripes(size);
    let transparent = vec![0u8; (size.width * size.height) as usize * 4];
    let mut semi_red = transparent.clone();
    for px in semi_red.chunks_exact_mut(4) {
        px.copy_from_slice(&[0, 0, 128, 128]);
    }
    let cases = [
        vec![EffectKind::Blur {
            kind: BlurKind::Box,
            radius_x: 5.0,
            radius_y: 2.0,
        }],
        vec![EffectKind::Stroke {
            color: Color { r: 255, g: 0, b: 0 },
            opacity: 0.8,
            thickness: 10.0,
            offset: -0.5,
            softness: StrokeSoftness::Bilinear,
        }],
        every_effect(),
    ];
    for effects in cases {
        let mut canvas = Canvas::headless(size).unwrap();
        canvas.add_layer_with_pixels("base", &backdrop).unwrap();
        adjust_with(&mut canvas, &[EffectKind::Blur {
            kind: BlurKind::Gaussian,
            radius_x: 2.0,
            radius_y: 2.0,
        }]);
        let target = canvas.add_layer_with_pixels("paint", &transparent).unwrap();
        canvas.add_layer_with_pixels("between", &semi_red).unwrap();
        adjust_with(&mut canvas, &effects);
        canvas.layers().set_active(Some(target));

        let brush = BrushEngine::new();
        brush.size.set(8.0);
        brush.opacity.set(1.0);
        let white = Color::new(255, 255, 255);
        for (x0, x1, y) in [(10.0, 80.0, 48.0), (60.0, 200.0, 30.0), (120.0, 130.0, 70.0)] {
            canvas.begin_stroke(white, 1.0, false).unwrap();
            canvas.stamp(|t| brush.begin_stroke(line_sample(x0, y, 0), white, t)).unwrap();
            canvas.stamp(|t| brush.push_sample(line_sample(x1, y, 10), t)).unwrap();
            canvas.stamp(|t| brush.end_stroke(t)).unwrap();
            canvas.commit_stroke().unwrap();
        }
        let patched = canvas.read_pixels().unwrap();
        canvas.recomposite().unwrap();
        let full = canvas.read_pixels().unwrap();
        let bbox = diff_bbox(&patched, &full, size.width, 0);
        assert_eq!(count_differing(&patched, &full, 0), 0, "{effects:?} differs within {bbox:?}");
    }
}

/// Per-event cost of the adjusted paths on a large canvas with every effect
/// on, for `--nocapture`: the preview below the adjustment, the mask preview,
/// the stroke commit, against a full rebuild.
#[test]
#[allow(clippy::print_stderr, clippy::cast_precision_loss)]
fn adjusted_stroke_costs() {
    use std::time::Instant;

    use oxiedraw_core::brush_engine::BrushEngine;

    let size = Size::new(2048, 2048);
    let mut canvas = Canvas::headless(size).unwrap();
    let base = canvas.add_layer_with_pixels("base", &half_stripes(size)).unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    let data = AdjustmentData {
        effects: every_effect().into_iter().map(Effect::new).collect(),
    };
    canvas.set_layer_effects(adj, data).unwrap();
    let brush = BrushEngine::new();
    brush.size.set(24.0);
    brush.opacity.set(1.0);

    let mut stroke = |target: usize, color: Color| {
        canvas.layers().set_active(Some(target));
        canvas.begin_stroke(color, 1.0, false).unwrap();
        canvas.stamp(|t| brush.begin_stroke(line_sample(100.0, 1000.0, 0), color, t)).unwrap();
        canvas.present().unwrap();
        let events = 60;
        let started = Instant::now();
        for i in 1..=events {
            let x = 100.0 + i as f32 * 12.0;
            canvas.stamp(|t| brush.push_sample(line_sample(x, 1000.0, i * 8), t)).unwrap();
            canvas.present().unwrap();
        }
        let per_event = started.elapsed() / events as u32;
        canvas.stamp(|t| brush.end_stroke(t)).unwrap();
        let started = Instant::now();
        canvas.commit_stroke().unwrap();
        (per_event, started.elapsed())
    };
    let (below_event, below_commit) = stroke(base, Color::new(255, 255, 255));
    let (mask_event, mask_commit) = stroke(adj, Color::BLACK);
    let started = Instant::now();
    canvas.recomposite().unwrap();
    let full = started.elapsed();

    eprintln!("2048^2, every effect: paint below {below_event:?}/event, commit {below_commit:?}");
    eprintln!("2048^2, every effect: paint mask {mask_event:?}/event, commit {mask_commit:?}");
    eprintln!("2048^2, every effect: full rebuild {full:?}");
}

fn adjust_with(canvas: &mut Canvas, effects: &[EffectKind]) {
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    let data = AdjustmentData {
        effects: effects.iter().copied().map(Effect::new).collect(),
    };
    canvas.set_layer_effects(adj, data).unwrap();
}

#[test]
fn incremental_blur_preview_matches_full() {
    let size = Size::new(96, 96);
    let stripes: Vec<u8> = (0..size.height)
        .flat_map(|y| {
            let v = if y / 2 % 2 == 0 { 100 } else { 150 };
            [v, v, v, 255].repeat(size.width as usize)
        })
        .collect();
    let blur = |kind| EffectKind::Blur {
        kind,
        radius_x: 4.0,
        radius_y: 4.0,
    };
    let cases = [
        vec![blur(BlurKind::Box)],
        vec![blur(BlurKind::Gaussian)],
        vec![EffectKind::sharpen_default()],
        vec![blur(BlurKind::Gaussian), EffectKind::sharpen_default()],
    ];
    for effects in cases {
        let mut canvas = Canvas::headless(size).unwrap();
        let base = canvas.add_layer_with_pixels("base", &stripes).unwrap();
        adjust_with(&mut canvas, &effects);
        let (incremental, full) = incremental_and_full_line(&mut canvas, base, Color::WHITE);

        let bbox = diff_bbox(&incremental, &full, size.width, 2);
        assert_eq!(count_differing(&incremental, &full, 2), 0, "{effects:?} differs within {bbox:?}");
    }
}

#[test]
fn incremental_blur_preview_includes_layers_between() {
    let size = Size::new(96, 96);
    let blur = EffectKind::Blur {
        kind: BlurKind::Gaussian,
        radius_x: 4.0,
        radius_y: 4.0,
    };
    for effects in [vec![blur], vec![blur, EffectKind::sharpen_default()]] {
        let mut canvas = Canvas::headless(size).unwrap();
        let base = canvas
            .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
            .unwrap();
        canvas
            .add_layer_with_pixels("between", &solid(size, 255, 0, 0))
            .unwrap();
        adjust_with(&mut canvas, &effects);
        let (incremental, full) = incremental_and_full_line(&mut canvas, base, Color::WHITE);

        let differing = incremental.iter().zip(&full).filter(|(a, b)| !near(**a, **b, 2)).count();
        assert_eq!(differing, 0, "{effects:?}");
    }
}

#[test]
fn incremental_blur_stroke_preview_matches_full() {
    let size = Size::new(256, 96);
    let blur = EffectKind::Blur {
        kind: BlurKind::Gaussian,
        radius_x: 3.0,
        radius_y: 3.0,
    };
    let stroke = |thickness| EffectKind::Stroke {
        color: Color { r: 255, g: 0, b: 0 },
        opacity: 1.0,
        thickness,
        offset: 1.0,
        softness: StrokeSoftness::Bilinear,
    };
    for effects in [vec![stroke(4.0)], vec![blur, stroke(4.0)], vec![blur, stroke(12.0)], every_effect()] {
        let mut canvas = Canvas::headless(size).unwrap();
        let base = canvas
            .add_layer_with_pixels("base", &vec![0u8; (size.width * size.height) as usize * 4])
            .unwrap();
        adjust_with(&mut canvas, &effects);
        let (incremental, full) = incremental_and_full_line(&mut canvas, base, Color::WHITE);

        let differing = incremental.iter().zip(&full).filter(|(a, b)| !near(**a, **b, 2)).count();
        assert_eq!(differing, 0, "{effects:?}");
    }
}

/// A red backdrop with a brightness-0 adjustment on top should composite to
/// black: the effect multiplies the whole backdrop down.
#[test]
fn adjustment_brightness_zero_blackens_backdrop() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    let out = canvas.read_pixels().unwrap();
    assert!(
        out[0] <= 3 && out[1] <= 3 && out[2] <= 3,
        "brightness 0 should blacken backdrop, got B{} G{} R{}",
        out[0],
        out[1],
        out[2]
    );
}

/// The identity adjustment (default Hue/Sat/Bright) must leave the backdrop
/// untouched - proves the mask-mix + copy-back round-trips losslessly.
#[test]
fn identity_adjustment_preserves_backdrop() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let base = solid(size, 40, 120, 200);
    canvas.add_layer_with_pixels("base", &base).unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(adj, one_effect(EffectKind::hue_sat_bright_identity()))
        .unwrap();

    let out = canvas.read_pixels().unwrap();
    for (i, (a, b)) in base.iter().zip(out.iter()).enumerate().take(4 * 8) {
        assert!(near(*a, *b, 3), "identity drifted at {i}: {a} vs {b}");
    }
}

/// A fully black mask gates the effect off: the brightness-0 adjustment should
/// have no visible result, leaving the red backdrop intact.
#[test]
fn black_mask_gates_off_effect() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    // Paint the whole mask black (no effect anywhere).
    canvas.clear_layer_at(adj, [0.0, 0.0, 0.0, 1.0]).unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    let out = canvas.read_pixels().unwrap();
    assert!(
        near(out[2], 255, 3) && out[0] <= 3 && out[1] <= 3,
        "black mask should keep red backdrop, got B{} G{} R{}",
        out[0],
        out[1],
        out[2]
    );
}

/// A disabled effect stays in the stack but must not change the backdrop.
#[test]
fn disabled_effect_is_noop() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    let mut effect = Effect::new(EffectKind::HueSatBright {
        hue_degrees: 0.0,
        saturation: 1.0,
        brightness: 0.0,
    });
    effect.enabled = false;
    canvas
        .set_layer_effects(adj, AdjustmentData { effects: vec![effect] })
        .unwrap();

    let out = canvas.read_pixels().unwrap();
    assert!(
        near(out[2], 255, 3),
        "disabled effect must not blacken, got R{}",
        out[2]
    );
}

/// Drawing on a normal layer *below* an adjustment layer must not show the
/// adjustment's (white) mask in the in-stroke preview - the preview should be
/// the unadjusted backdrop, not white.
#[test]
fn stroke_preview_below_adjustment_does_not_show_mask() {
    use oxiedraw_core::color::Color;

    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let base = canvas
        .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    // A real (identity-ish) effect so the slot carries an effect stack.
    canvas
        .set_layer_effects(adj, one_effect(EffectKind::hue_sat_bright_identity()))
        .unwrap();

    // Draw on the base layer, below the adjustment.
    canvas.layers().set_active(Some(base));
    canvas
        .begin_stroke(Color { r: 255, g: 255, b: 255 }, 1.0, false)
        .unwrap();
    let preview = canvas.read_pixels().unwrap();

    // The preview must still be the red backdrop, not the white adjustment mask.
    assert!(
        preview[2] > 200 && preview[0] < 60 && preview[1] < 60,
        "adjustment mask leaked into the preview: B{} G{} R{}",
        preview[0],
        preview[1],
        preview[2]
    );
}

/// Committing a stroke on a layer below an adjustment must rebuild the canvas
/// through the adjustment-aware path: the effect stays applied, and the white
/// mask is never baked into the composite. (Regression: commit_stroke_into_layer
/// used the non-adjustment composite, so pointer-up painted the mask.)
#[test]
fn commit_stroke_below_adjustment_keeps_effect() {
    use oxiedraw_core::color::Color;

    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let base = canvas
        .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    // Stroke on the base layer and commit (pointer-up) - the commit rebuilds
    // the canvas, which is the path under test.
    canvas.layers().set_active(Some(base));
    canvas
        .begin_stroke(Color { r: 255, g: 255, b: 255 }, 1.0, false)
        .unwrap();
    canvas.commit_stroke().unwrap();

    // After commit the brightness-0 adjustment must still blacken the backdrop;
    // the composite must never bake in the white mask.
    let out = canvas.read_pixels().unwrap();
    assert!(
        out[0] <= 4 && out[1] <= 4 && out[2] <= 4,
        "effect lost / mask baked after commit: B{} G{} R{}",
        out[0],
        out[1],
        out[2]
    );
}

/// Painting a layer *above* an adjustment must still show the adjusted
/// backdrop in the preview (the effect is baked into the cached below-stack),
/// not the original un-effected layers.
#[test]
fn preview_above_adjustment_keeps_backdrop_adjusted() {
    use oxiedraw_core::color::Color;

    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();
    // Transparent layer on top of the adjustment to paint on.
    let top = canvas
        .add_layer_with_pixels("top", &vec![0u8; (size.width * size.height) as usize * 4])
        .unwrap();

    canvas.layers().set_active(Some(top));
    canvas
        .begin_stroke(Color { r: 255, g: 255, b: 255 }, 1.0, false)
        .unwrap();
    let preview = canvas.read_pixels().unwrap();

    // The red base, adjusted by the brightness-0 layer below `top`, must read
    // black through the transparent top - not the original red.
    assert!(
        preview[0] <= 4 && preview[1] <= 4 && preview[2] <= 4,
        "backdrop not adjusted when painting above the adjustment: B{} G{} R{}",
        preview[0],
        preview[1],
        preview[2]
    );
}

/// Live effect preview: while a stroke is in flight on a layer below an
/// adjustment, the previewed canvas must already show the effect applied (the
/// backdrop blackened), not the unadjusted layer.
#[test]
fn live_preview_below_adjustment_shows_effect() {
    use oxiedraw_core::color::Color;

    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let base = canvas
        .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    canvas.layers().set_active(Some(base));
    canvas
        .begin_stroke(Color { r: 255, g: 255, b: 255 }, 1.0, false)
        .unwrap();
    // read_pixels mid-stroke runs the adjusted preview path.
    let preview = canvas.read_pixels().unwrap();
    assert!(
        preview[0] <= 4 && preview[1] <= 4 && preview[2] <= 4,
        "effect not previewed live below adjustment: B{} G{} R{}",
        preview[0],
        preview[1],
        preview[2]
    );
}

/// Same, but the adjustment is *below* the painted layer (exercises the
/// cached below-stack path rather than the above-loop).
#[test]
fn stroke_preview_above_adjustment_does_not_show_mask() {
    use oxiedraw_core::color::Color;

    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(adj, one_effect(EffectKind::hue_sat_bright_identity()))
        .unwrap();
    // A transparent top layer to paint on, above the adjustment.
    let top = canvas
        .add_layer_with_pixels("top", &vec![0u8; (size.width * size.height) as usize * 4])
        .unwrap();

    canvas.layers().set_active(Some(top));
    canvas
        .begin_stroke(Color { r: 255, g: 255, b: 255 }, 1.0, false)
        .unwrap();
    let preview = canvas.read_pixels().unwrap();

    // The adjustment (with its white mask) sits in the cached below-stack; it
    // must not paint the mask, so the red base shows through the empty top.
    assert!(
        preview[2] > 200 && preview[0] < 60 && preview[1] < 60,
        "adjustment mask leaked into the below-cache: B{} G{} R{}",
        preview[0],
        preview[1],
        preview[2]
    );
}

/// Stroke an opaque square sitting on a transparent backdrop: with an outside
/// offset, pixels just beyond the silhouette edge should pick up the stroke
/// color, while the far corners stay transparent.
#[test]
fn stroke_colors_the_silhouette_edge() {
    let size = Size::new(32, 32);
    let mut canvas = Canvas::headless(size).unwrap();

    // Transparent canvas with an opaque green 8x8 square at (12,12)..(20,20).
    let mut base = vec![0u8; (size.width * size.height) as usize * 4];
    for y in 12..20 {
        for x in 12..20 {
            let i = ((y * size.width + x) * 4) as usize;
            base[i..i + 4].copy_from_slice(&[0, 255, 0, 255]);
        }
    }
    canvas.add_layer_with_pixels("base", &base).unwrap();

    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::Stroke {
                color: Color { r: 255, g: 0, b: 0 },
                opacity: 1.0,
                thickness: 4.0,
                offset: 1.0, // fully outside
                softness: StrokeSoftness::Pixelated,
            }),
        )
        .unwrap();

    let out = canvas.read_pixels().unwrap();
    let at = |x: u32, y: u32| {
        let i = ((y * size.width + x) * 4) as usize;
        (out[i], out[i + 1], out[i + 2], out[i + 3])
    };

    // A pixel just outside the left edge of the square should be reddish.
    let (b, g, r, a) = at(10, 16);
    assert!(
        a > 100 && r > 120 && g < 120 && b < 120,
        "expected stroke color just outside the edge, got B{b} G{g} R{r} A{a}"
    );

    // A far corner stays transparent (no edge nearby to stroke).
    let (_, _, _, corner_a) = at(0, 0);
    assert!(corner_a < 16, "far corner should stay transparent, got A{corner_a}");
}

/// A thick centred stroke over a straight half-plane edge: exercises the
/// jump-flood band at a radius where the old per-pixel disc scan was slowest.
/// Backdrop: left half (x < 32) opaque white, right half transparent, so the
/// silhouette edge is the vertical line x = 32. A centred thickness-16 stroke
/// must paint a ~16px-wide red band straddling that line, leaving the deep
/// interior white and the far exterior transparent.
#[test]
fn thick_stroke_band_spans_silhouette_edge() {
    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();

    let mut base = vec![0u8; (size.width * size.height) as usize * 4];
    for y in 0..size.height {
        for x in 0..size.width / 2 {
            let i = ((y * size.width + x) * 4) as usize;
            base[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    canvas.add_layer_with_pixels("base", &base).unwrap();

    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::Stroke {
                color: Color { r: 255, g: 0, b: 0 },
                opacity: 1.0,
                thickness: 16.0,
                offset: 0.0, // centred on the edge
                softness: StrokeSoftness::Pixelated,
            }),
        )
        .unwrap();

    let out = canvas.read_pixels().unwrap();
    let at = |x: u32, y: u32| {
        let i = ((y * size.width + x) * 4) as usize;
        (out[i], out[i + 1], out[i + 2], out[i + 3])
    };
    let is_red = |(b, g, r, a): (u8, u8, u8, u8)| a > 150 && r > 150 && g < 100 && b < 100;

    // Just inside (x=30) and just outside (x=34) the edge: inside the band.
    assert!(is_red(at(30, 32)), "inside-edge pixel not in band: {:?}", at(30, 32));
    assert!(is_red(at(34, 32)), "outside-edge pixel not in band: {:?}", at(34, 32));

    // Deep interior stays white (no band reaches it).
    let (b, g, r, a) = at(5, 32);
    assert!(
        a > 200 && r > 200 && g > 200 && b > 200,
        "deep interior should stay white, got B{b} G{g} R{r} A{a}"
    );
    // Far exterior stays transparent.
    let (_, _, _, far_a) = at(60, 32);
    assert!(far_a < 16, "far exterior should stay transparent, got A{far_a}");
}

/// Build BGRA8 pixels: opaque red in the left half (`x < width/2`), transparent
/// elsewhere. Used to tell folder-scoped from global adjustments.
fn left_half_red(size: Size) -> Vec<u8> {
    let mut px = vec![0u8; (size.width * size.height) as usize * 4];
    for y in 0..size.height {
        for x in 0..size.width / 2 {
            let i = ((y * size.width + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
    }
    px
}

/// An adjustment inside a folder must affect only the folder's contents, not
/// layers below the folder. Stack (bottom->top): blue A (outside), folder { red
/// left-half B, brightness-0 adjustment }. Brightness 0 blackens the folder
/// accumulator; A below the folder must stay blue where B is transparent.
#[test]
fn adjustment_is_clipped_to_its_folder() {
    use oxiedraw_core::document::{LayerGroup, LayerTreeNode};

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let a = canvas
        .add_layer_with_pixels("A-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let b = canvas
        .add_layer_with_pixels("B-red", &left_half_red(size))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    // Ids in canvas order (bottom-first): A at root, then a folder holding B+adj.
    let snap = canvas.layers().snapshot();
    let (id_a, id_b, id_adj) = (snap[a].id.clone(), snap[b].id.clone(), snap[adj].id.clone());
    let folded = vec![
        LayerTreeNode::layer(id_a),
        LayerTreeNode::Group(LayerGroup {
            id: "g1".to_string(),
            name: "Folder".to_string(),
            expanded: true,
            children: vec![LayerTreeNode::layer(id_b), LayerTreeNode::layer(id_adj)],
        }),
    ];
    canvas.set_layer_tree(folded).unwrap();

    let at = |out: &[u8], x: u32, y: u32| {
        let i = ((y * size.width + x) * 4) as usize;
        (out[i], out[i + 1], out[i + 2])
    };

    let out = canvas.read_pixels().unwrap();
    // Left half: B was opaque red, blackened by the folder's adjustment.
    let (lb, lg, lr) = at(&out, 16, 32);
    assert!(
        lb <= 6 && lg <= 6 && lr <= 6,
        "folder content should be blackened, got B{lb} G{lg} R{lr}"
    );
    // Right half: only A (blue) shows. The folder's adjustment must NOT reach it.
    let (rb, rg, rr) = at(&out, 48, 32);
    assert!(
        rb > 200 && rg <= 12 && rr <= 12,
        "layer below the folder must stay blue, got B{rb} G{rg} R{rr}"
    );

    // Flattening the tree (no folders) returns to global scope: the adjustment
    // now blackens A too, so the right half goes black.
    canvas.set_layer_tree(Vec::new()).unwrap();
    let flat = canvas.read_pixels().unwrap();
    let (rb2, rg2, rr2) = at(&flat, 48, 32);
    assert!(
        rb2 <= 6 && rg2 <= 6 && rr2 <= 6,
        "without a folder the adjustment should reach A, got B{rb2} G{rg2} R{rr2}"
    );
}

/// Dragging the bottom layer out of a folder that holds an adjustment keeps the
/// flat order, so only the tree changes; the canvas must still pick up that the
/// layer left the adjustment's scope.
#[test]
fn moving_a_layer_out_of_an_adjusted_folder_recomposites() {
    use oxiedraw_core::document::{LayerGroup, LayerTreeNode};

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let bottom = canvas
        .add_layer_with_pixels("Layer 3", &solid(size, 255, 0, 0))
        .unwrap();
    let background = canvas
        .add_layer_with_pixels("Background", &left_half_red(size))
        .unwrap();
    let adj = canvas.add_adjustment_layer("Adjustment").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();
    canvas.layers().set_active(Some(adj));
    let snap = canvas.layers().snapshot();
    let ids = [snap[bottom].id.clone(), snap[background].id.clone(), snap[adj].id.clone()];
    let group = |children: Vec<LayerTreeNode>| {
        LayerTreeNode::Group(LayerGroup {
            id: "g1".to_string(),
            name: "Group".to_string(),
            expanded: true,
            children,
        })
    };
    let at = |out: &[u8], x: u32, y: u32| {
        let i = ((y * size.width + x) * 4) as usize;
        (out[i], out[i + 1], out[i + 2])
    };

    // Everything grouped: the adjustment blackens both layers.
    let all = ids.iter().cloned().map(LayerTreeNode::layer).collect();
    canvas.set_layer_tree(vec![group(all)]).unwrap();
    let (b, g, r) = at(&canvas.read_pixels().unwrap(), 48, 32);
    assert!(b <= 6 && g <= 6 && r <= 6, "grouped: expected black, got B{b} G{g} R{r}");

    // The bottom layer leaves the folder; the flat order is unchanged.
    canvas.layers().set_active(Some(background));
    let rest = ids[1..].iter().cloned().map(LayerTreeNode::layer).collect();
    canvas
        .set_layer_tree(vec![LayerTreeNode::layer(ids[0].clone()), group(rest)])
        .unwrap();
    let out = canvas.read_pixels().unwrap();
    let (b, g, r) = at(&out, 48, 32);
    assert!(b > 200 && g <= 12 && r <= 12, "moved out: expected blue, got B{b} G{g} R{r}");
    let display = canvas.present().map(|_| canvas.read_display().unwrap()).unwrap();
    let (b, g, r) = at(&display, 48, 32);
    assert!(b > 200 && g <= 12 && r <= 12, "display: expected blue, got B{b} G{g} R{r}");
}

/// The same drag with every effect on, after a mask stroke painted through the
/// live path while everything was grouped: the display must match a document
/// that painted the same stroke straight in the final layout.
#[test]
fn moving_a_layer_out_of_an_adjusted_folder_matches_a_fresh_document() {
    use oxiedraw_core::brush_engine::BrushEngine;
    use oxiedraw_core::document::{LayerGroup, LayerTreeNode};

    let size = Size::new(256, 96);
    let build = |grouped_first: bool| -> Vec<u8> {
        let mut canvas = Canvas::headless(size).unwrap();
        let bottom = canvas
            .add_layer_with_pixels("Layer 3", &half_stripes(size))
            .unwrap();
        let background = canvas
            .add_layer_with_pixels("Background", &left_half_red(size))
            .unwrap();
        let adj = canvas.add_adjustment_layer("Adjustment").unwrap();
        let data = AdjustmentData {
            effects: every_effect().into_iter().map(Effect::new).collect(),
        };
        canvas.set_layer_effects(adj, data).unwrap();
        let snap = canvas.layers().snapshot();
        let ids = [snap[bottom].id.clone(), snap[background].id.clone(), snap[adj].id.clone()];
        let group = |children: Vec<LayerTreeNode>| {
            LayerTreeNode::Group(LayerGroup {
                id: "g1".to_string(),
                name: "Group".to_string(),
                expanded: true,
                children,
            })
        };
        let all = ids.iter().cloned().map(LayerTreeNode::layer).collect();
        let rest = ids[1..].iter().cloned().map(LayerTreeNode::layer).collect();
        let moved_out = vec![LayerTreeNode::layer(ids[0].clone()), group(rest)];
        if grouped_first {
            canvas.set_layer_tree(vec![group(all)]).unwrap();
        } else {
            canvas.set_layer_tree(moved_out.clone()).unwrap();
        }
        // Paint the mask through the live path, then commit.
        canvas.layers().set_active(Some(adj));
        let brush = BrushEngine::new();
        brush.size.set(10.0);
        canvas.begin_stroke(Color::BLACK, 1.0, false).unwrap();
        canvas.stamp(|t| brush.begin_stroke(line_sample(20.0, 40.0, 0), Color::BLACK, t)).unwrap();
        canvas.present().unwrap();
        canvas.stamp(|t| brush.push_sample(line_sample(90.0, 40.0, 10), t)).unwrap();
        canvas.present().unwrap();
        canvas.stamp(|t| brush.end_stroke(t)).unwrap();
        canvas.commit_stroke().unwrap();
        canvas.present().unwrap();
        if grouped_first {
            canvas.set_layer_tree(moved_out).unwrap();
        }
        canvas.layers().set_active(Some(background));
        canvas.present().unwrap();
        canvas.read_display().unwrap()
    };
    let moved = build(true);
    let fresh = build(false);
    let bbox = diff_bbox(&moved, &fresh, size.width, 2);
    assert_eq!(count_differing(&moved, &fresh, 2), 0, "moved-out layout differs within {bbox:?}");
}

/// The stroke-commit path (not just recompose) must respect folder scoping:
/// painting on a layer inside a folder with a brightness-0 adjustment must not
/// blacken a layer sitting below the folder.
#[test]
fn committed_stroke_respects_folder_scope() {
    use oxiedraw_core::brush_engine::{BrushEngine, InputSample};
    use oxiedraw_core::document::{LayerGroup, LayerTreeNode};
    use oxiedraw_utils::geometry::Point;

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let a = canvas
        .add_layer_with_pixels("A-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let paint = canvas
        .add_layer_with_pixels("paint", &vec![0u8; (size.width * size.height) as usize * 4])
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    let snap = canvas.layers().snapshot();
    let tree = vec![
        LayerTreeNode::layer(snap[a].id.clone()),
        LayerTreeNode::Group(LayerGroup {
            id: "g1".to_string(),
            name: "Folder".to_string(),
            expanded: true,
            children: vec![
                LayerTreeNode::layer(snap[paint].id.clone()),
                LayerTreeNode::layer(snap[adj].id.clone()),
            ],
        }),
    ];
    canvas.set_layer_tree(tree).unwrap();

    // Stroke a short red dab in the left half on the paint layer, then commit.
    let brush = BrushEngine::new();
    brush.size.set(10.0);
    brush.opacity.set(1.0);
    let red = Color::new(255, 0, 0);
    canvas.layers().set_active(Some(paint));
    canvas.begin_stroke(red, 1.0, false).unwrap();
    let s = |x: f32, y: f32, t: u64| InputSample {
        position: Point::new(x, y),
        pressure: 1.0,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rotation: 0.0,
        time_ms: t,
    };
    canvas.stamp(|t| brush.begin_stroke(s(14.0, 32.0, 0), red, t)).unwrap();
    canvas.stamp(|t| brush.push_sample(s(18.0, 32.0, 10), t)).unwrap();
    canvas.stamp(|t| brush.end_stroke(t)).unwrap();
    canvas.commit_stroke().unwrap();

    let out = canvas.read_pixels().unwrap();
    let at = |x: u32, y: u32| {
        let i = ((y * size.width + x) * 4) as usize;
        (out[i], out[i + 1], out[i + 2])
    };
    // Right half: only A (blue) shows; the folder's adjustment must not reach it.
    let (rb, rg, rr) = at(48, 32);
    assert!(
        rb > 200 && rg <= 12 && rr <= 12,
        "committed stroke leaked the folder adjustment onto A: B{rb} G{rg} R{rr}"
    );
}

/// The LIVE in-stroke preview must also respect folder scoping: while painting
/// inside a folder with a brightness-0 adjustment, the previewed canvas must not
/// blacken a layer below the folder (matches what the commit will produce).
#[test]
fn live_preview_respects_folder_scope() {
    use oxiedraw_core::brush_engine::{BrushEngine, InputSample};
    use oxiedraw_core::document::{LayerGroup, LayerTreeNode};
    use oxiedraw_utils::geometry::Point;

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let a = canvas
        .add_layer_with_pixels("A-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let paint = canvas
        .add_layer_with_pixels("paint", &vec![0u8; (size.width * size.height) as usize * 4])
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    let snap = canvas.layers().snapshot();
    let tree = vec![
        LayerTreeNode::layer(snap[a].id.clone()),
        LayerTreeNode::Group(LayerGroup {
            id: "g1".to_string(),
            name: "Folder".to_string(),
            expanded: true,
            children: vec![
                LayerTreeNode::layer(snap[paint].id.clone()),
                LayerTreeNode::layer(snap[adj].id.clone()),
            ],
        }),
    ];
    canvas.set_layer_tree(tree).unwrap();

    // Begin a stroke on the paint layer (inside the folder) and read the live
    // preview WITHOUT committing.
    let brush = BrushEngine::new();
    brush.size.set(10.0);
    brush.opacity.set(1.0);
    let red = Color::new(255, 0, 0);
    canvas.layers().set_active(Some(paint));
    canvas.begin_stroke(red, 1.0, false).unwrap();
    let s = |x: f32, y: f32, t: u64| InputSample {
        position: Point::new(x, y),
        pressure: 1.0,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rotation: 0.0,
        time_ms: t,
    };
    canvas.stamp(|t| brush.begin_stroke(s(14.0, 32.0, 0), red, t)).unwrap();
    canvas.stamp(|t| brush.push_sample(s(18.0, 32.0, 10), t)).unwrap();

    let out = canvas.read_pixels().unwrap();
    let i = ((32 * size.width + 48) * 4) as usize; // right half, only A shows
    let (rb, rg, rr) = (out[i], out[i + 1], out[i + 2]);
    assert!(
        rb > 200 && rg <= 12 && rr <= 12,
        "live preview leaked the folder adjustment onto A: B{rb} G{rg} R{rr}"
    );
}

/// The live TRANSFORM preview must run the adjustment chain (folder-scoped):
/// transforming a layer inside a folder with a brightness-0 adjustment must
/// blacken the transformed content but not a layer below the folder.
#[test]
fn transform_preview_respects_folder_scope() {
    use oxiedraw_core::document::{LayerGroup, LayerTreeNode};
    use oxiedraw_utils::geometry::TransformRect;

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let a = canvas
        .add_layer_with_pixels("A-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let target = canvas
        .add_layer_with_pixels("target", &left_half_red(size))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    let snap = canvas.layers().snapshot();
    let tree = vec![
        LayerTreeNode::layer(snap[a].id.clone()),
        LayerTreeNode::Group(LayerGroup {
            id: "g1".to_string(),
            name: "Folder".to_string(),
            expanded: true,
            children: vec![
                LayerTreeNode::layer(snap[target].id.clone()),
                LayerTreeNode::layer(snap[adj].id.clone()),
            ],
        }),
    ];
    canvas.set_layer_tree(tree).unwrap();

    // Start an identity transform on the target (warped == source).
    canvas.layers().set_active(Some(target));
    canvas
        .begin_transform_preview_gpu(&[(target, &left_half_red(size), 64, 64)])
        .unwrap();
    let rect = TransformRect { cx: 32.0, cy: 32.0, w: 64.0, h: 64.0, angle: 0.0 };
    canvas.set_transform_preview(rect, rect, 64, 64);

    let out = canvas.read_transform_preview().unwrap();
    let at = |x: u32, y: u32| {
        let i = ((y * size.width + x) * 4) as usize;
        (out[i], out[i + 1], out[i + 2])
    };
    // Left half: transformed red content, blackened by the folder adjustment.
    let (lb, lg, lr) = at(16, 32);
    assert!(
        lb <= 6 && lg <= 6 && lr <= 6,
        "transformed content should be adjusted (blackened), got B{lb} G{lg} R{lr}"
    );
    // Right half: only A (blue) shows; the folder adjustment must not reach it.
    let (rb, rg, rr) = at(48, 32);
    assert!(
        rb > 200 && rg <= 12 && rr <= 12,
        "transform preview leaked the folder adjustment onto A: B{rb} G{rg} R{rr}"
    );
}

/// Transform preview runs the adjustment chain even without folders: an
/// adjustment above the transformed layer (flat stack) must adjust the warped
/// content live.
#[test]
fn transform_preview_applies_flat_adjustment() {
    use oxiedraw_utils::geometry::TransformRect;

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let target = canvas
        .add_layer_with_pixels("target", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    canvas.layers().set_active(Some(target));
    canvas
        .begin_transform_preview_gpu(&[(target, &solid(size, 0, 0, 255), 64, 64)])
        .unwrap();
    let rect = TransformRect { cx: 32.0, cy: 32.0, w: 64.0, h: 64.0, angle: 0.0 };
    canvas.set_transform_preview(rect, rect, 64, 64);

    let out = canvas.read_transform_preview().unwrap();
    assert!(
        out[0] <= 6 && out[1] <= 6 && out[2] <= 6,
        "flat adjustment must blacken the transformed layer, got B{} G{} R{}",
        out[0], out[1], out[2]
    );
}

/// A multi-layer (group) transform must also run the adjustment chain live: an
/// adjustment above two transformed layers adjusts the whole warped stack.
#[test]
fn transform_preview_applies_adjustment_multi_target() {
    use oxiedraw_utils::geometry::TransformRect;

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let a = canvas
        .add_layer_with_pixels("a", &solid(size, 0, 0, 255))
        .unwrap();
    let b = canvas
        .add_layer_with_pixels("b", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    // Transform both content layers together (identity), adjustment above both.
    canvas
        .begin_transform_preview_gpu(&[
            (a, &solid(size, 0, 0, 255), 64, 64),
            (b, &solid(size, 0, 0, 255), 64, 64),
        ])
        .unwrap();
    let rect = TransformRect { cx: 32.0, cy: 32.0, w: 64.0, h: 64.0, angle: 0.0 };
    canvas.set_transform_preview(rect, rect, 64, 64);

    let out = canvas.read_transform_preview().unwrap();
    assert!(
        out[0] <= 6 && out[1] <= 6 && out[2] <= 6,
        "adjustment must blacken the multi-target transform, got B{} G{} R{}",
        out[0], out[1], out[2]
    );
}

// Deleting a selection on an adjustment layer must refill the hole with white
// (full effect), not leave transparency. Mask slots stay opaque black-gray-white.
#[test]
fn delete_selection_refills_adjustment_mask_white() {
    let (mut canvas, adj) = adjustment_with_left_selection();
    canvas.erase_selection_in_layer(adj).unwrap();
    assert_mask_white(&mut canvas, adj);
}

// The cut path (clear_selection_from_layer) must also refill the hole white.
#[test]
fn cut_selection_refills_adjustment_mask_white() {
    let (mut canvas, adj) = adjustment_with_left_selection();
    canvas.clear_selection_from_layer(adj).unwrap();
    assert_mask_white(&mut canvas, adj);
}

// The selection-move lift (extract_selection_pixels) must also refill white.
#[test]
fn lift_selection_refills_adjustment_mask_white() {
    let (mut canvas, adj) = adjustment_with_left_selection();
    canvas.extract_selection_pixels(adj).unwrap();
    assert_mask_white(&mut canvas, adj);
}

/// Headless canvas with one adjustment layer (active) and the left half selected.
fn adjustment_with_left_selection() -> (Canvas, usize) {
    use oxiedraw_core::selection::{RectShape, SelectionShape};
    use oxiedraw_core::tools::SelectionMode;

    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas.layers().set_active(Some(adj));
    #[allow(clippy::cast_precision_loss)]
    canvas
        .apply_selection_shape(
            &SelectionShape::Rect(RectShape {
                x: 0.0,
                y: 0.0,
                w: (size.width / 2) as f32,
                h: size.height as f32,
            }),
            SelectionMode::Replace,
        )
        .unwrap();
    (canvas, adj)
}

/// The whole 16x16 mask must read opaque white (deleted region refilled, rest
/// untouched).
fn assert_mask_white(canvas: &mut Canvas, adj: usize) {
    let out = canvas.read_layer(adj).unwrap();
    let px = |x: usize, y: usize| {
        let i = (y * 16 + x) * 4;
        (out[i], out[i + 1], out[i + 2], out[i + 3])
    };
    assert_eq!(px(2, 8), (255, 255, 255, 255), "deleted region must be white");
    assert_eq!(px(12, 8), (255, 255, 255, 255), "kept region stays white");
}

/// Painting an adjustment layer's mask with the grayscale mask view OFF must
/// preview the EFFECT (the adjusted backdrop), never the black/white mask. With
/// a darkening effect over a red backdrop and the default white mask, stroking
/// the mask should keep the canvas a darkened red - not flash the white mask.
#[test]
fn mask_edit_preview_shows_effect_not_mask() {
    use oxiedraw_core::brush_engine::{BrushEngine, InputSample};
    use oxiedraw_utils::geometry::Point;

    let sample = |x: f32, y: f32, t: u64| InputSample {
        position: Point::new(x, y),
        pressure: 1.0,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rotation: 0.0,
        time_ms: t,
    };

    let size = Size::new(32, 32);
    let mut canvas = Canvas::headless(size).unwrap();
    let _base = canvas
        .add_layer_with_pixels("base", &solid(size, 0, 0, 255))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.5, // darken the red backdrop
            }),
        )
        .unwrap();

    // The committed (no-stroke) canvas is the darkened red the effect produces.
    let committed = canvas.read_pixels().unwrap();
    let center = ((16 * 32) + 16) * 4;
    assert!(
        committed[center + 2] < 220 && committed[center + 2] > committed[center] + 30,
        "expected a darkened-red committed canvas, got B{} G{} R{}",
        committed[center],
        committed[center + 1],
        committed[center + 2],
    );

    // Paint the adjustment's own mask (mask view left OFF). The default mask is
    // white, so the white brush leaves it unchanged - the preview must equal the
    // committed adjusted result, not the grayscale mask.
    canvas.layers().set_active(Some(adj));
    let brush = BrushEngine::new();
    brush.size.set(8.0);
    brush.opacity.set(1.0);
    let white = Color::new(255, 255, 255);
    canvas.begin_stroke(white, 1.0, false).unwrap();
    canvas
        .stamp(|t| brush.begin_stroke(sample(16.0, 16.0, 0), white, t))
        .unwrap();
    let preview = canvas.read_pixels().unwrap();

    // Effect result, not the mask: still red-dominant and close to committed.
    assert!(
        preview[center + 2] > preview[center] + 30,
        "mask leaked into the preview (gray, not red): B{} G{} R{}",
        preview[center],
        preview[center + 1],
        preview[center + 2],
    );
    assert!(
        near(preview[center + 2], committed[center + 2], 6),
        "painted preview diverged from the committed effect: {} vs {}",
        preview[center + 2],
        committed[center + 2],
    );
}

/// Live-stroke scoped preview for a target OUTSIDE a folder-bounded adjustment.
/// Stack (bottom->top): blue C (root), folder { red-left B, brightness-0 adj },
/// transparent target T (root, on top). While stroking T the folder's adjustment
/// must blacken only the folder content; C below stays blue (the flat fast path
/// used to bleed the effect onto everything below the adjustment). The second
/// preview frame reuses the cached static folder and must match the first.
#[test]
fn live_scoped_preview_clips_folder_and_cache_matches() {
    use oxiedraw_core::color::Color;
    use oxiedraw_core::document::{LayerGroup, LayerTreeNode};

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let c = canvas
        .add_layer_with_pixels("C-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let b = canvas
        .add_layer_with_pixels("B-red", &left_half_red(size))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();
    let t = canvas
        .add_layer_with_pixels("T", &vec![0u8; (size.width * size.height) as usize * 4])
        .unwrap();

    let snap = canvas.layers().snapshot();
    let tree = vec![
        LayerTreeNode::layer(snap[c].id.clone()),
        LayerTreeNode::Group(LayerGroup {
            id: "g1".to_string(),
            name: "Folder".to_string(),
            expanded: true,
            children: vec![
                LayerTreeNode::layer(snap[b].id.clone()),
                LayerTreeNode::layer(snap[adj].id.clone()),
            ],
        }),
        LayerTreeNode::layer(snap[t].id.clone()),
    ];
    canvas.set_layer_tree(tree).unwrap();

    canvas.layers().set_active(Some(t));
    canvas
        .begin_stroke(Color { r: 255, g: 255, b: 255 }, 1.0, false)
        .unwrap();

    let at = |out: &[u8], x: u32, y: u32| {
        let i = ((y * size.width + x) * 4) as usize;
        (out[i], out[i + 1], out[i + 2])
    };

    let frame1 = canvas.read_pixels().unwrap(); // builds the folder cache
    let frame2 = canvas.read_pixels().unwrap(); // reuses the folder cache

    // Left half: folder content (red B) blackened by the brightness-0 adjustment.
    let (lb, lg, lr) = at(&frame1, 16, 32);
    assert!(
        lb <= 6 && lg <= 6 && lr <= 6,
        "folder content should be blackened, got B{lb} G{lg} R{lr}"
    );
    // Right half: C (blue) below the folder must NOT be touched by the effect.
    let (rb, rg, rr) = at(&frame1, 48, 32);
    assert!(
        rb > 200 && rg <= 12 && rr <= 12,
        "adjustment bled onto the layer below its folder, got B{rb} G{rg} R{rr}"
    );

    // The cached second frame must be pixel-identical to the first.
    assert!(
        frame1 == frame2,
        "cached scoped preview frame diverged from the first (uncached) frame"
    );
}

/// A live FILTER preview on a layer OUTSIDE a folder-bounded adjustment must
/// keep the adjustment clipped to its folder. Stack (bottom->top): blue C
/// (root, the filtered target), folder { red-left B, brightness-0 adj }. The
/// flat filter preview ignored adjustment slots and folder scope, so the
/// brightness-0 adjustment bled onto everything below it (blackening C) until
/// the filter was applied. With scoped routing, C stays blue (identity HSV) and
/// only the folder content B is blackened.
#[test]
fn filter_preview_respects_folder_scope() {
    use oxiedraw_core::document::{LayerGroup, LayerTreeNode};
    use oxiedraw_core::filters::FilterSpec;

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let c = canvas
        .add_layer_with_pixels("C-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let b = canvas
        .add_layer_with_pixels("B-red", &left_half_red(size))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    let snap = canvas.layers().snapshot();
    let tree = vec![
        LayerTreeNode::layer(snap[c].id.clone()),
        LayerTreeNode::Group(LayerGroup {
            id: "g1".to_string(),
            name: "Folder".to_string(),
            expanded: true,
            children: vec![
                LayerTreeNode::layer(snap[b].id.clone()),
                LayerTreeNode::layer(snap[adj].id.clone()),
            ],
        }),
    ];
    canvas.set_layer_tree(tree).unwrap();

    // Arm an identity HSV filter on C (below/outside the adjustment's folder).
    canvas.begin_filter(&[c], FilterSpec::hsv_identity());
    let frame = canvas.read_filter_preview().unwrap();

    let at = |out: &[u8], x: u32, y: u32| {
        let i = ((y * size.width + x) * 4) as usize;
        (out[i], out[i + 1], out[i + 2])
    };

    // Left half: folder content (red B) blackened by the brightness-0 adjustment.
    let (lb, lg, lr) = at(&frame, 16, 32);
    assert!(
        lb <= 6 && lg <= 6 && lr <= 6,
        "folder content should be blackened, got B{lb} G{lg} R{lr}"
    );
    // Right half: C (blue), the filtered target below the folder, must keep its
    // color - the adjustment must NOT bleed onto it.
    let (rb, rg, rr) = at(&frame, 48, 32);
    assert!(
        rb > 200 && rg <= 12 && rr <= 12,
        "adjustment bled onto the filtered layer below its folder, got B{rb} G{rg} R{rr}"
    );
}

/// A live filter preview over SEVERAL selected layers must still run the
/// adjustment chain. The flat preview it used to fall back to for anything but
/// a single target has no notion of adjustment slots: it dropped every effect
/// and composited the adjustment's mask as if it were pixels, so arming the
/// filter turned the canvas white until the filter was applied.
#[test]
fn filter_preview_keeps_adjustment_with_several_targets() {
    use oxiedraw_core::filters::FilterSpec;

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let base = canvas
        .add_layer_with_pixels("base-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let second = canvas
        .add_layer_with_pixels("second", &left_half_red(size))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    canvas.begin_filter(&[base, second], FilterSpec::hsv_identity());
    let frame = canvas.read_filter_preview().unwrap();

    // Brightness 0 over the whole canvas: an identity filter under it changes
    // nothing, so every pixel stays black.
    for (i, px) in frame.chunks_exact(4).enumerate() {
        assert!(
            px[0] <= 6 && px[1] <= 6 && px[2] <= 6,
            "pixel {i} not blackened by the adjustment: B{} G{} R{}",
            px[0],
            px[1],
            px[2]
        );
    }
}

/// `replace_all_layers` resets every slot to a plain raster layer, so restoring
/// the saved kinds afterwards (undo/redo, project load, component edit) has to
/// push an adjustment's effect stack back to the renderer. Restoring the kind on
/// the layer state alone left the effect off the GPU slot and composited the
/// mask as pixels - a white sheet over the canvas.
#[test]
fn restored_adjustment_kind_still_composites() {
    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .add_layer_with_pixels("base-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();
    let before = canvas.read_pixels().unwrap();

    let snapshot = canvas.layers().snapshot();
    let mut entries = Vec::with_capacity(snapshot.len());
    let mut kinds = Vec::with_capacity(snapshot.len());
    for (i, l) in snapshot.iter().enumerate() {
        let pixels = canvas.read_layer(i).unwrap();
        entries.push((l.id.clone(), l.name.clone(), l.visible, l.blend, l.opacity, pixels));
        kinds.push(l.kind.clone());
    }
    canvas.replace_all_layers(&entries).unwrap();
    for (i, kind) in kinds.into_iter().enumerate() {
        canvas.set_layer_kind(i, kind).unwrap();
    }

    let after = canvas.read_pixels().unwrap();
    for (i, (a, b)) in before.iter().zip(after.iter()).enumerate() {
        assert!(
            near(*a, *b, 2),
            "byte {i} changed across the layer restore: {a} vs {b}"
        );
    }
}

/// An adjustment layer's slot holds its mask, so a filter armed on it filters
/// the mask - and the live preview has to show the effect gated by that filtered
/// mask, the way apply + recomposite will. The flat preview instead dropped the
/// effect and pushed the raw white mask on screen.
#[test]
fn filter_preview_on_the_adjustment_layer_filters_its_mask() {
    use oxiedraw_core::filters::FilterSpec;

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    let base = canvas
        .add_layer_with_pixels("base-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();
    // Default mask is white everywhere, so the effect blackens the blue base.
    let composite = canvas.read_pixels().unwrap();
    assert!(composite[0] <= 6, "expected a blackened canvas to start from");

    // An identity filter on the mask leaves the composite alone.
    canvas.begin_filter(&[adj], FilterSpec::hsv_identity());
    let frame = canvas.read_filter_preview().unwrap();
    for (i, (a, b)) in composite.iter().zip(frame.iter()).enumerate() {
        assert!(near(*a, *b, 2), "byte {i}: preview {b} != composite {a}");
    }

    // Inverting the mask turns it black, which gates the effect off entirely:
    // the base layer previews unadjusted (blue).
    canvas.update_filter(FilterSpec::Invert);
    let frame = canvas.read_filter_preview().unwrap();
    let base_pixels = canvas.read_layer(base).unwrap();
    for (i, (a, b)) in base_pixels.iter().zip(frame.iter()).enumerate() {
        assert!(
            near(*a, *b, 2),
            "byte {i}: inverted mask should gate the effect off, {b} != {a}"
        );
    }

    // ...and applying it gives the same picture the preview promised.
    canvas.apply_filter(&[adj], FilterSpec::Invert).unwrap();
    let applied = canvas.read_pixels().unwrap();
    for (i, (a, b)) in frame.iter().zip(applied.iter()).enumerate() {
        assert!(near(*a, *b, 2), "byte {i}: apply {b} != preview {a}");
    }
}

/// With the mask toggled into view the canvas shows that mask, so a filter armed
/// on its layer must preview on the mask - not swap the canvas back to the
/// composite while the user drags the sliders.
#[test]
fn filter_preview_with_the_mask_in_view_filters_what_is_on_screen() {
    use oxiedraw_core::filters::FilterSpec;

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .add_layer_with_pixels("base-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();

    let adj_id = canvas.layers().snapshot()[adj].id.clone();
    canvas.set_mask_view(Some(adj_id));
    canvas.present().unwrap();
    let mask_view = canvas.read_display().unwrap();
    assert!(
        mask_view[0] >= 250 && mask_view[3] >= 250,
        "expected the white mask on screen, got {:?}",
        &mask_view[..4]
    );

    // Inverting turns the white mask black. The composite (blue blackened by the
    // effect) is opaque black too, so check a mid-gray filter as well: only the
    // mask can go gray, the composite stays black.
    canvas.begin_filter(&[adj], FilterSpec::Invert);
    canvas.present().unwrap();
    let inverted = canvas.read_display().unwrap();
    assert!(
        inverted[0] <= 6 && inverted[3] >= 250,
        "inverted mask should be black and opaque, got {:?}",
        &inverted[..4]
    );

    canvas.update_filter(FilterSpec::Hsv {
        hue_degrees: 0.0,
        saturation: 1.0,
        value: 0.5,
    });
    canvas.present().unwrap();
    let dimmed = canvas.read_display().unwrap();
    assert!(
        dimmed[0] > 20 && dimmed[0] < 235,
        "expected the dimmed mask on screen, got {:?}",
        &dimmed[..4]
    );
}

/// Cropping rebuilds the renderer and reloads every layer as a plain raster one.
/// The kinds are restored afterwards, and adjustment layers have to reach the
/// GPU slot too - otherwise the effect is gone and its white mask composites as
/// pixels, whiting out the canvas.
#[test]
fn cropping_keeps_adjustment_layers_working() {
    use oxiedraw_core::tools::CropRect;

    let size = Size::new(64, 64);
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .add_layer_with_pixels("base-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            one_effect(EffectKind::HueSatBright {
                hue_degrees: 0.0,
                saturation: 1.0,
                brightness: 0.0,
            }),
        )
        .unwrap();
    let before = canvas.read_pixels().unwrap();
    assert!(before[0] <= 6, "expected a blackened canvas to start from");

    canvas.apply_crop(CropRect::new(0.0, 0.0, 32.0, 32.0)).unwrap();

    assert!(
        canvas.layer_effects(adj).is_some(),
        "crop dropped the adjustment kind"
    );
    let after = canvas.read_pixels().unwrap();
    for (i, px) in after.chunks_exact(4).enumerate() {
        assert!(
            px[0] <= 6 && px[1] <= 6 && px[2] <= 6,
            "pixel {i} lost the adjustment across the crop: {px:?}"
        );
    }
}

#[test]
fn gaussian_blur_adjustment_matches_the_filter() {
    let size = Size::new(32, 32);
    let effect = |kind| EffectKind::Blur {
        kind,
        radius_x: 6.0,
        radius_y: 6.0,
    };
    let adjusted_with = |kind| {
        let mut canvas = Canvas::headless(size).unwrap();
        canvas.add_layer_with_pixels("base", &left_half_red(size)).unwrap();
        let adj = canvas.add_adjustment_layer("adj").unwrap();
        canvas.set_layer_effects(adj, one_effect(effect(kind))).unwrap();
        canvas.read_pixels().unwrap()
    };
    let adjusted = adjusted_with(BlurKind::Gaussian);

    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("base", &left_half_red(size)).unwrap();
    let spec = effect(BlurKind::Gaussian).as_filter_spec().unwrap();
    canvas.apply_filter(&[idx], spec).unwrap();
    let filtered = canvas.read_pixels().unwrap();

    let mismatches = adjusted.iter().zip(&filtered).filter(|(a, b)| !near(**a, **b, 2)).count();
    assert_eq!(mismatches, 0);
    let boxed = adjusted_with(BlurKind::Box);
    assert!(adjusted.iter().zip(&boxed).any(|(a, b)| !near(*a, *b, 8)));
}

fn curves_effect(channel: CurveChannel, level: u8) -> EffectKind {
    let mut curves = CurveSet::default();
    *curves.curve_mut(channel) =
        Curve::from_points(&[CurvePoint::new(0, level), CurvePoint::new(255, level)]);
    EffectKind::Curves { curves }
}

#[test]
fn curves_adjustment_maps_the_backdrop() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .add_layer_with_pixels("base", &solid(size, 40, 120, 200))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(adj, one_effect(curves_effect(CurveChannel::Red, 30)))
        .unwrap();

    let out = canvas.read_pixels().unwrap();
    assert!(
        near(out[2], 30, 1) && near(out[1], 120, 1) && near(out[0], 40, 1),
        "got B{} G{} R{}",
        out[0],
        out[1],
        out[2]
    );
}

#[test]
fn live_preview_below_curves_adjustments_shows_both() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let base = canvas
        .add_layer_with_pixels("base", &solid(size, 40, 120, 200))
        .unwrap();
    let red = canvas.add_adjustment_layer("red").unwrap();
    canvas
        .set_layer_effects(red, one_effect(curves_effect(CurveChannel::Red, 30)))
        .unwrap();
    let blue = canvas.add_adjustment_layer("blue").unwrap();
    canvas
        .set_layer_effects(blue, one_effect(curves_effect(CurveChannel::Blue, 220)))
        .unwrap();

    canvas.layers().set_active(Some(base));
    canvas
        .begin_stroke(Color { r: 255, g: 255, b: 255 }, 1.0, false)
        .unwrap();
    let preview = canvas.read_pixels().unwrap();
    assert!(
        near(preview[2], 30, 1) && near(preview[0], 220, 1) && near(preview[1], 120, 1),
        "batched preview lost a curve: B{} G{} R{}",
        preview[0],
        preview[1],
        preview[2]
    );
}

#[test]
fn curves_histogram_sees_the_folder_backdrop_inside_the_mask() {
    use oxiedraw_core::document::{LayerGroup, LayerTreeNode};

    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let a = canvas
        .add_layer_with_pixels("A-blue", &solid(size, 255, 0, 0))
        .unwrap();
    let b = canvas
        .add_layer_with_pixels("B-red", &left_half_red(size))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(adj, one_effect(curves_effect(CurveChannel::Rgb, 0)))
        .unwrap();
    let snap = canvas.layers().snapshot();
    canvas
        .set_layer_tree(vec![
            LayerTreeNode::layer(snap[a].id.clone()),
            LayerTreeNode::Group(LayerGroup {
                id: "g1".to_string(),
                name: "Folder".to_string(),
                expanded: true,
                children: vec![
                    LayerTreeNode::layer(snap[b].id.clone()),
                    LayerTreeNode::layer(snap[adj].id.clone()),
                ],
            }),
        ])
        .unwrap();

    let half = 8 * 16;
    let h = canvas.adjustment_histogram(adj).unwrap();
    assert_eq!(h.channel(CurveChannel::Red)[255], half);
    assert_eq!(h.channel(CurveChannel::Blue)[255], 0, "layer below the folder counted");
    assert_eq!(h.channel(CurveChannel::Rgb).iter().sum::<u32>(), 3 * half);

    let mut mask = solid(size, 255, 255, 255);
    mask[..(16 * 8 * 4)].copy_from_slice(&solid(Size::new(16, 8), 0, 0, 0));
    canvas.restore_layer(adj, &mask).unwrap();
    let masked = canvas.adjustment_histogram(adj).unwrap();
    assert_eq!(masked.channel(CurveChannel::Red)[255], half / 2);

    let out = canvas.read_pixels().unwrap();
    assert!(near(out[(16 * 2) * 4 + 2], 255, 1), "masked-off red stays red");
    assert!(out[(16 * 12) * 4 + 2] <= 1, "adjusted red went black");
}

#[test]
fn curves_histogram_of_a_clipped_adjustment_stays_on_its_base() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    canvas
        .add_layer_with_pixels("paper-blue", &solid(size, 255, 0, 0))
        .unwrap();
    canvas
        .add_layer_with_pixels("base-red", &left_half_red(size))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(adj, one_effect(curves_effect(CurveChannel::Rgb, 0)))
        .unwrap();
    canvas.set_layer_clipped(adj, true).unwrap();

    let h = canvas.adjustment_histogram(adj).unwrap();
    assert_eq!(h.channel(CurveChannel::Red)[255], 8 * 16);
    assert_eq!(h.channel(CurveChannel::Blue)[255], 0, "paper outside the base counted");
}
