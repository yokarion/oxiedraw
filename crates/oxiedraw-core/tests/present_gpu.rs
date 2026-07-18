//! GPU integration tests for the present colour-space conversion, driven
//! through the public `Canvas` API. Each test is `#[ignore]` because it needs a
//! working Vulkan loader + device; run with
//! `cargo test -p oxiedraw-core --test present_gpu -- --ignored`.
//!
//! The canvas holds premultiplied linear; the display dmabuf has to hold
//! premultiplied gamma for GSK to composite it over the checker correctly.
//! `present_convert.frag` does that conversion, and these pin it down.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::canvas::Canvas;
use oxiedraw_utils::color::{linear_to_srgb, srgb_to_linear};
use oxiedraw_utils::geometry::Size;

const SIZE: Size = Size {
    width: 8,
    height: 8,
};

/// Build a full-canvas BGRA8 buffer where every pixel is `(b, g, r, a)`.
fn filled(b: u8, g: u8, r: u8, a: u8) -> Vec<u8> {
    let n = (SIZE.width * SIZE.height) as usize;
    let mut px = Vec::with_capacity(n * 4);
    for _ in 0..n {
        px.extend_from_slice(&[b, g, r, a]);
    }
    px
}

/// Present the current state and read the display dmabuf back.
fn present_and_read(canvas: &mut Canvas) -> Vec<u8> {
    canvas.present().unwrap();
    canvas.read_display().unwrap()
}

/// What the present pass should emit for a canvas byte `v` at alpha `a`.
fn expected_gamma_premul(v: u8, a: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    let alpha = f32::from(a) / 255.0;
    let straight_linear = srgb_to_linear(v) / alpha;
    let gamma = f32::from(linear_to_srgb(straight_linear)) / 255.0;
    (gamma * alpha * 255.0).round() as u8
}

fn near(a: u8, b: u8, tol: i32) -> bool {
    (i32::from(a) - i32::from(b)).abs() <= tol
}

/// At alpha 1 both premultiplication conventions agree, so the display byte
/// should match the canvas byte.
#[test]
#[ignore = "requires vulkan loader and device"]
fn present_leaves_opaque_pixels_unchanged() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("opaque", &filled(40, 130, 220, 255))
        .unwrap();

    let display = present_and_read(&mut canvas);
    for px in display.chunks_exact(4) {
        assert!(near(px[0], 40, 1), "B {} != 40", px[0]);
        assert!(near(px[1], 130, 1), "G {} != 130", px[1]);
        assert!(near(px[2], 220, 1), "R {} != 220", px[2]);
        assert_eq!(px[3], 255, "alpha must stay opaque");
    }
}

/// Fully transparent pixels stay zeroed, so GTK shows the checker through them.
#[test]
#[ignore = "requires vulkan loader and device"]
fn present_keeps_transparent_pixels_zero() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("empty", &filled(0, 0, 0, 0))
        .unwrap();

    let display = present_and_read(&mut canvas);
    assert!(
        display.chunks_exact(4).all(|px| px == [0, 0, 0, 0]),
        "transparent canvas must present as all-zero"
    );
}

/// A semi-transparent pixel must reach the display re-premultiplied in gamma
/// space, i.e. darker than the canvas byte. Revert the present to a plain copy
/// and the byte stays put, which is what clamped to white over the checker.
#[test]
#[ignore = "requires vulkan loader and device"]
fn present_regamma_premultiplies_semitransparent_pixels() {
    // Half-alpha white as the GPU stores it: srgb(1.0 * 0.5) = 188.
    let canvas_byte = linear_to_srgb(0.5);
    assert_eq!(canvas_byte, 188);

    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels(
            "half",
            &filled(canvas_byte, canvas_byte, canvas_byte, 128),
        )
        .unwrap();

    // A lone Normal layer over a cleared canvas composites to itself.
    let composited = canvas.read_pixels().unwrap();
    assert!(
        near(composited[0], canvas_byte, 2),
        "canvas should hold the linear-premultiplied byte, got {}",
        composited[0]
    );

    let display = present_and_read(&mut canvas);
    let want = expected_gamma_premul(canvas_byte, 128);
    for px in display.chunks_exact(4) {
        for (i, ch) in px[..3].iter().enumerate() {
            assert!(
                near(*ch, want, 2),
                "channel {i}: display {ch} != expected gamma-premul {want}"
            );
        }
        assert!(near(px[3], 128, 1), "alpha must pass through");
    }

    // Half-alpha white lands near 128, well under the canvas's 188.
    assert!(
        i32::from(canvas_byte) - i32::from(display[0]) > 20,
        "conversion did not run ({canvas_byte} -> {})",
        display[0]
    );
}

/// Semi-transparent colour (not just grey) converts per channel, and alpha is
/// carried through untouched.
#[test]
#[ignore = "requires vulkan loader and device"]
fn present_converts_each_channel_independently() {
    let (b, g, r, a) = (60u8, 190u8, 255u8, 90u8);
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("tint", &filled(b, g, r, a))
        .unwrap();

    let display = present_and_read(&mut canvas);
    let want = [
        expected_gamma_premul(b, a),
        expected_gamma_premul(g, a),
        expected_gamma_premul(r, a),
    ];
    for px in display.chunks_exact(4) {
        for i in 0..3 {
            assert!(
                near(px[i], want[i], 2),
                "channel {i}: display {} != expected {}",
                px[i],
                want[i]
            );
        }
        assert!(near(px[3], a, 1), "alpha {} != {a}", px[3]);
    }
}

/// Presenting twice must be stable - the render pass discards and fully
/// redraws the buffer, so a second present cannot double-convert.
#[test]
#[ignore = "requires vulkan loader and device"]
fn present_is_idempotent() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("half", &filled(188, 188, 188, 128))
        .unwrap();

    let first = present_and_read(&mut canvas);
    let second = present_and_read(&mut canvas);
    assert_eq!(first, second, "repeated presents must be stable");
}

/// The in-stroke present only rewrites the dab's dirty region, so pixels an
/// earlier present wrote outside the current clip must survive. A regression
/// shows up as earlier dabs vanishing mid-stroke.
#[test]
#[ignore = "requires vulkan loader and device"]
fn incremental_present_preserves_pixels_outside_the_clip() {
    use oxiedraw_core::brush_engine::{BrushEngine, InputSample};
    use oxiedraw_core::color::Color;
    use oxiedraw_utils::geometry::{Point, Size};

    const BIG: Size = Size { width: 256, height: 256 };
    const RED: Color = Color::new(255, 0, 0);
    const START: f32 = 40.0;
    const END: f32 = 200.0;

    let dab = |x: f32, y: f32, t: u64| InputSample {
        position: Point::new(x, y),
        pressure: 1.0,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rotation: 0.0,
        time_ms: t,
    };

    let brush = BrushEngine::new();
    brush.size.set(16.0);
    brush.opacity.set(1.0);

    let mut canvas = Canvas::headless(BIG).unwrap();
    canvas.add_layer("paint").unwrap();
    canvas.begin_stroke(RED, 1.0, false).unwrap();

    // Walk a horizontal line, presenting per sample - the real drag loop. Many
    // samples so the stabilizer actually tracks the input to the far end.
    canvas.stamp(|t| brush.begin_stroke(dab(START, START, 0), RED, t)).unwrap();
    canvas.present().unwrap();
    let steps: u32 = 120;
    for i in 1..=steps {
        let f = f64::from(i) as f32 / f64::from(steps) as f32;
        let x = START + f * (END - START);
        canvas.stamp(|t| brush.push_sample(dab(x, START, u64::from(i) * 8), t)).unwrap();
        canvas.present().unwrap();
    }

    // Alpha survives the gamma conversion untouched, so the display must match
    // the preview it converts from; a wrongly skipped region shows up as stale.
    let preview = canvas.read_pixels().unwrap();
    let display = canvas.read_display().unwrap();
    assert_eq!(preview.len(), display.len());

    let mut mismatches = 0u32;
    let mut first_bad = None;
    for i in (0..display.len()).step_by(4) {
        if display[i + 3] != preview[i + 3] {
            mismatches += 1;
            if first_bad.is_none() {
                let px = (i / 4) as u32;
                first_bad = Some((px % BIG.width, px / BIG.width, preview[i + 3], display[i + 3]));
            }
        }
    }
    assert_eq!(
        mismatches, 0,
        "display diverged from preview in {mismatches} px; first at {first_bad:?} (x, y, want, got)",
    );

    // Sanity: the stroke travelled, so the check above saw disjoint dirty
    // regions rather than a blank canvas.
    let alpha_at = |x: u32, y: u32| display[((y * BIG.width + x) * 4 + 3) as usize];
    assert!(alpha_at(START as u32, START as u32) > 200, "start of stroke never drawn");
    assert!(alpha_at(END as u32 - 4, START as u32) > 200, "stroke did not reach the far end");
    assert_eq!(alpha_at(150, 200), 0, "pixels no dab covered must stay transparent");
}
