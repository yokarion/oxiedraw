//! GPU integration tests for the layer filters, driven through the public
//! `Canvas` API.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::curves::{Curve, CurveChannel, CurvePoint, CurveSet};
use oxiedraw_core::enum_meta::EnumMeta;
use oxiedraw_core::filters::{BlurKind, FilterSpec, apply_cpu};
use oxiedraw_core::selection::{RectShape, SelectionShape};
use oxiedraw_core::tools::SelectionMode;
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

#[test]
fn invert_opaque_red_to_cyan() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 0, 0, 255)).unwrap();

    canvas.apply_filter(&[idx], FilterSpec::Invert).unwrap();
    let out = canvas.read_layer(idx).unwrap();

    // Opaque red (B0 G0 R255) inverts to cyan (B255 G255 R0).
    assert!(near(out[0], 255, 2), "B={}", out[0]);
    assert!(near(out[1], 255, 2), "G={}", out[1]);
    assert!(near(out[2], 0, 2), "R={}", out[2]);
    assert_eq!(out[3], 255, "alpha preserved");
}

#[test]
fn invert_twice_restores_original() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let src = solid(size, 40, 120, 200);
    let idx = canvas.add_layer_with_pixels("t", &src).unwrap();

    canvas.apply_filter(&[idx], FilterSpec::Invert).unwrap();
    canvas.apply_filter(&[idx], FilterSpec::Invert).unwrap();
    let out = canvas.read_layer(idx).unwrap();

    for (a, b) in src.iter().zip(out.iter()) {
        assert!(near(*a, *b, 2), "double invert drifted: {a} vs {b}");
    }
}

#[test]
fn hsv_value_zero_blackens() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 30, 150, 220)).unwrap();

    canvas
        .apply_filter(
            &[idx],
            FilterSpec::Hsv {
                hue_degrees: 0.0,
                saturation: 1.0,
                value: 0.0,
            },
        )
        .unwrap();
    let out = canvas.read_layer(idx).unwrap();

    assert!(out[0] <= 3 && out[1] <= 3 && out[2] <= 3, "value 0 => black");
    assert_eq!(out[3], 255, "alpha preserved");
}

const fn blur(kind: BlurKind, radius: f32) -> FilterSpec {
    FilterSpec::Blur {
        kind,
        radius_x: radius,
        radius_y: radius,
    }
}

fn white_spike(size: Size) -> (Vec<u8>, usize) {
    let mut px = vec![0u8; (size.width * size.height) as usize * 4];
    let center = ((size.height / 2 * size.width + size.width / 2) * 4) as usize;
    px[center..center + 4].copy_from_slice(&[255, 255, 255, 255]);
    (px, center)
}

#[test]
fn blur_spreads_a_spike() {
    let size = Size::new(8, 8);
    let (px, center) = white_spike(size);
    for &kind in BlurKind::ALL {
        let mut canvas = Canvas::headless(size).unwrap();
        let idx = canvas.add_layer_with_pixels("t", &px).unwrap();
        canvas.apply_filter(&[idx], blur(kind, 2.0)).unwrap();
        let out = canvas.read_layer(idx).unwrap();

        assert!(out[center + 3] < 255, "{kind:?}: center alpha should drop");
        assert!(out[center + 4 + 3] > 0, "{kind:?}: neighbor should gain coverage");
    }
}

#[test]
fn gaussian_blur_alpha_matches_cpu_reference() {
    let size = Size::new(24, 24);
    let (px, center) = white_spike(size);
    let spec = blur(BlurKind::Gaussian, 5.0);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &px).unwrap();
    canvas.apply_filter(&[idx], spec).unwrap();
    let gpu = canvas.read_layer(idx).unwrap();
    let cpu = apply_cpu(spec, &px, size.width, size.height, None);

    for (i, (g, c)) in gpu.chunks_exact(4).zip(cpu.chunks_exact(4)).enumerate() {
        assert!(near(g[3], c[3], 1), "alpha at {i}: gpu {} cpu {}", g[3], c[3]);
    }
    let boxed = apply_cpu(blur(BlurKind::Box, 5.0), &px, size.width, size.height, None);
    assert!(gpu[center + 3] > boxed[center + 3] + 2);
}

#[test]
fn sharpen_flat_color_is_identity() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let src = solid(size, 60, 90, 120);
    let idx = canvas.add_layer_with_pixels("t", &src).unwrap();

    canvas.apply_filter(&[idx], FilterSpec::Sharpen { amount: 3.0 }).unwrap();
    let out = canvas.read_layer(idx).unwrap();

    for (a, b) in src.iter().zip(out.iter()) {
        assert!(near(*a, *b, 2), "sharpen of flat color changed it: {a} vs {b}");
    }
}

#[test]
fn filter_respects_selection_mask() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 0, 0, 255)).unwrap();

    // Select only the left 8 columns, then invert.
    canvas
        .apply_selection_shape(
            &SelectionShape::Rect(RectShape {
                x: 0.0,
                y: 0.0,
                w: 8.0,
                h: 16.0,
            }),
            SelectionMode::Replace,
        )
        .unwrap();
    canvas.apply_filter(&[idx], FilterSpec::Invert).unwrap();
    let out = canvas.read_layer(idx).unwrap();

    let px = |x: usize, y: usize| {
        let i = (y * 16 + x) * 4;
        [out[i], out[i + 1], out[i + 2], out[i + 3]]
    };
    // Inside selection (left): inverted red -> cyan.
    let inside = px(2, 8);
    assert!(near(inside[0], 255, 4) && near(inside[2], 0, 4), "inside not inverted: {inside:?}");
    // Outside selection (right): untouched red.
    let outside = px(12, 8);
    assert!(near(outside[0], 0, 4) && near(outside[2], 255, 4), "outside changed: {outside:?}");
}

#[test]
fn live_preview_then_apply_and_cancel() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let src = solid(size, 0, 0, 255);
    let idx = canvas.add_layer_with_pixels("t", &src).unwrap();

    // Arm the preview and drive a present (this exercises the multi-submit
    // filter-preview compositor) without touching the layer pixels.
    canvas.begin_filter(&[idx], FilterSpec::Invert);
    let _ = canvas.present().unwrap();
    assert!(canvas.filter_active());
    let mid = canvas.read_layer(idx).unwrap();
    assert_eq!(mid, src, "preview must not modify the layer image");

    // Cancel leaves the layer untouched.
    canvas.cancel_filter();
    assert!(!canvas.filter_active());
    assert_eq!(canvas.read_layer(idx).unwrap(), src);

    // Re-arm and apply for real.
    canvas.begin_filter(&[idx], FilterSpec::Invert);
    let _ = canvas.present().unwrap();
    canvas.apply_filter(&[idx], FilterSpec::Invert).unwrap();
    assert!(!canvas.filter_active());
    let out = canvas.read_layer(idx).unwrap();
    assert!(near(out[0], 255, 2) && near(out[2], 0, 2), "apply after preview failed: {:?}", &out[..4]);
}

#[test]
fn filter_applies_to_multiple_layers() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let a = canvas.add_layer_with_pixels("a", &solid(size, 0, 0, 255)).unwrap();
    let b = canvas.add_layer_with_pixels("b", &solid(size, 255, 0, 0)).unwrap();

    canvas.apply_filter(&[a, b], FilterSpec::Invert).unwrap();

    let oa = canvas.read_layer(a).unwrap();
    let ob = canvas.read_layer(b).unwrap();
    // a: red -> cyan; b: blue -> yellow.
    assert!(near(oa[0], 255, 2) && near(oa[2], 0, 2), "layer a not inverted");
    assert!(near(ob[0], 0, 2) && near(ob[2], 255, 2), "layer b not inverted");
}

#[test]
fn brightness_lifts_a_fully_saturated_pixel() {
    // Regression: HSV value-multiply could not brighten a pixel already at
    // full value (e.g. pure red). Brightness > 1 now lifts additively, so a
    // saturated pixel still moves toward white.
    let size = Size::new(8, 8);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 0, 0, 255)).unwrap(); // opaque red

    canvas
        .apply_filter(
            &[idx],
            FilterSpec::Hsv {
                hue_degrees: 0.0,
                saturation: 1.0,
                value: 2.0,
            },
        )
        .unwrap();
    let out = canvas.read_layer(idx).unwrap();

    // Red channel stays high; the other channels lift up out of black.
    assert!(out[2] > 200, "R stays bright, got {}", out[2]);
    assert!(out[0] > 60 && out[1] > 60, "B/G must lift toward white: {} {}", out[0], out[1]);
}

#[test]
fn brightness_one_is_identity() {
    let size = Size::new(8, 8);
    let mut canvas = Canvas::headless(size).unwrap();
    let src = solid(size, 40, 120, 200);
    let idx = canvas.add_layer_with_pixels("t", &src).unwrap();
    canvas
        .apply_filter(
            &[idx],
            FilterSpec::Hsv {
                hue_degrees: 0.0,
                saturation: 1.0,
                value: 1.0,
            },
        )
        .unwrap();
    let out = canvas.read_layer(idx).unwrap();
    for (a, b) in src.iter().zip(out.iter()) {
        assert!(near(*a, *b, 2), "brightness 1.0 not identity: {a} vs {b}");
    }
}

#[test]
fn sharpen_is_visible_on_a_soft_edge() {
    // A smooth horizontal gradient (the kind of soft edge digital art has).
    // Unsharp must visibly change it, not only hard pixel steps.
    let w = 32usize;
    let size = Size::new(w as u32, w as u32);
    let mut canvas = Canvas::headless(size).unwrap();
    let mut px = vec![0u8; w * w * 4];
    for y in 0..w {
        for x in 0..w {
            let i = (y * w + x) * 4;
            #[allow(clippy::cast_possible_truncation)]
            let v = (x * 255 / (w - 1)) as u8;
            px[i] = v;
            px[i + 1] = v;
            px[i + 2] = v;
            px[i + 3] = 255;
        }
    }
    let idx = canvas.add_layer_with_pixels("t", &px).unwrap();
    let before = canvas.read_layer(idx).unwrap();
    canvas.apply_filter(&[idx], FilterSpec::Sharpen { amount: 2.0 }).unwrap();
    let after = canvas.read_layer(idx).unwrap();

    let max_delta = before
        .iter()
        .zip(after.iter())
        .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
        .max()
        .unwrap_or(0);
    assert!(max_delta > 8, "sharpen barely changed a soft gradient (max delta {max_delta})");
}

fn flat_curve(level: u8) -> Curve {
    Curve::from_points(&[CurvePoint::new(0, level), CurvePoint::new(255, level)])
}

fn inverted_curve() -> Curve {
    Curve::from_points(&[CurvePoint::new(0, 255), CurvePoint::new(255, 0)])
}

#[test]
fn curves_invert_flips_srgb_levels() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 40, 120, 200)).unwrap();

    let curves = CurveSet {
        rgb: inverted_curve(),
        ..CurveSet::default()
    };
    canvas.apply_filter(&[idx], FilterSpec::Curves { curves }).unwrap();
    let out = canvas.read_layer(idx).unwrap();

    assert!(near(out[0], 215, 2), "B={}", out[0]);
    assert!(near(out[1], 135, 2), "G={}", out[1]);
    assert!(near(out[2], 55, 2), "R={}", out[2]);
    assert_eq!(out[3], 255, "alpha preserved");
}

#[test]
fn channel_curve_runs_before_the_master_curve() {
    let size = Size::new(8, 8);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 40, 120, 200)).unwrap();

    let curves = CurveSet {
        rgb: inverted_curve(),
        red: flat_curve(0),
        ..CurveSet::default()
    };
    canvas.apply_filter(&[idx], FilterSpec::Curves { curves }).unwrap();
    let out = canvas.read_layer(idx).unwrap();

    assert!(near(out[2], 255, 1), "invert(red(200)) = invert(0), got R={}", out[2]);
    assert!(near(out[1], 135, 1), "G={}", out[1]);
}

#[test]
fn layers_histogram_counts_inside_the_selection() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 0, 0, 255)).unwrap();

    let whole = canvas.layers_histogram(&[idx]).unwrap();
    assert_eq!(whole.channel(CurveChannel::Red)[255], 16 * 16);

    canvas
        .apply_selection_shape(
            &SelectionShape::Rect(RectShape {
                x: 0.0,
                y: 0.0,
                w: 8.0,
                h: 16.0,
            }),
            SelectionMode::Replace,
        )
        .unwrap();
    let selected = canvas.layers_histogram(&[idx]).unwrap();
    assert_eq!(selected.channel(CurveChannel::Red)[255], 8 * 16);
}

#[test]
fn identity_curves_leave_the_layer_alone() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let mut src = solid(size, 40, 120, 200);
    let half = oxiedraw_utils::color::linear_to_srgb(0.5 * 0.25);
    src[..4].copy_from_slice(&[half, half, half, 128]);
    let idx = canvas.add_layer_with_pixels("t", &src).unwrap();

    let spec = FilterSpec::Curves {
        curves: CurveSet::default(),
    };
    canvas.apply_filter(&[idx], spec).unwrap();
    let out = canvas.read_layer(idx).unwrap();

    for (i, (a, b)) in src.iter().zip(out.iter()).enumerate() {
        assert!(near(*a, *b, 1), "identity curves drifted at {i}: {a} vs {b}");
    }
}

#[test]
fn channel_curves_only_touch_their_channel() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 40, 120, 200)).unwrap();

    let curves = CurveSet {
        green: flat_curve(10),
        ..CurveSet::default()
    };
    canvas.apply_filter(&[idx], FilterSpec::Curves { curves }).unwrap();
    let out = canvas.read_layer(idx).unwrap();

    assert!(near(out[0], 40, 1) && near(out[2], 200, 1), "B={} R={}", out[0], out[2]);
    assert!(near(out[1], 10, 1), "G={}", out[1]);
}

#[test]
fn many_distinct_curves_each_read_their_own_row() {
    let size = Size::new(8, 8);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 0, 0, 0)).unwrap();

    for step in 0..40u8 {
        let level = 60 + step * 4;
        let curves = CurveSet {
            red: flat_curve(level),
            ..CurveSet::default()
        };
        canvas.apply_filter(&[idx], FilterSpec::Curves { curves }).unwrap();
        let out = canvas.read_layer(idx).unwrap();
        assert!(near(out[2], level, 1), "step {step}: R={} want {level}", out[2]);
    }
}

#[test]
fn curves_preview_matches_apply() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 40, 120, 200)).unwrap();

    let curves = CurveSet {
        rgb: Curve::from_points(&[
            CurvePoint::new(0, 0),
            CurvePoint::new(90, 160),
            CurvePoint::new(255, 255),
        ]),
        ..CurveSet::default()
    };
    let spec = FilterSpec::Curves { curves };
    canvas.begin_filter(&[idx], FilterSpec::Curves { curves: CurveSet::default() });
    canvas.update_filter(spec);
    let preview = canvas.read_filter_preview().unwrap();
    canvas.apply_filter(&[idx], spec).unwrap();
    let applied = canvas.read_pixels().unwrap();

    assert!(!near(preview[1], 120, 5), "preview did not change G={}", preview[1]);
    for (a, b) in preview.iter().zip(applied.iter()) {
        assert!(near(*a, *b, 1), "preview {a} vs applied {b}");
    }
}
