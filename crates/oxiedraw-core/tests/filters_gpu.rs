//! GPU integration tests for the layer filters, driven through the public
//! `Canvas` API. Each test is `#[ignore]` because it needs a working Vulkan
//! loader + device; run with `cargo test -p oxiedraw-core --test filters_gpu
//! -- --ignored`.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::filters::FilterSpec;
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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

#[test]
#[ignore = "requires vulkan loader and device"]
fn blur_spreads_a_spike() {
    let size = Size::new(8, 8);
    let mut canvas = Canvas::headless(size).unwrap();
    let mut px = vec![0u8; (size.width * size.height) as usize * 4];
    let center = (4 * 8 + 4) * 4;
    px[center..center + 4].copy_from_slice(&[255, 255, 255, 255]);
    let idx = canvas.add_layer_with_pixels("t", &px).unwrap();

    canvas
        .apply_filter(
            &[idx],
            FilterSpec::BoxBlur {
                radius_x: 2.0,
                radius_y: 2.0,
            },
        )
        .unwrap();
    let out = canvas.read_layer(idx).unwrap();

    assert!(out[center + 3] < 255, "center alpha should drop after blur");
    // A neighbor within the blur radius should pick up some energy.
    let neighbor = (4 * 8 + 5) * 4;
    assert!(out[neighbor + 3] > 0, "neighbor should gain coverage");
}

#[test]
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
