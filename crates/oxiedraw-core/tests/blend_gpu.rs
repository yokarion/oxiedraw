//! GPU integration tests for per-layer blend modes, driven through the public
//! `Canvas` API. Each test is `#[ignore]` because it needs a working Vulkan
//! loader + device; run with
//! `cargo test -p oxiedraw-core --test blend_gpu -- --ignored`.
//!
//! These pin down stack ordering: a layer with a non-Normal blend mode must
//! still composite at its own position in the stack, not sink below the layers
//! underneath it.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::document::BlendMode;
use oxiedraw_utils::geometry::Size;

const SIZE: Size = Size {
    width: 8,
    height: 8,
};

/// Full-canvas BGRA8 buffer where every pixel is `(b, g, r, a)`.
fn filled(b: u8, g: u8, r: u8, a: u8) -> Vec<u8> {
    let n = (SIZE.width * SIZE.height) as usize;
    let mut px = Vec::with_capacity(n * 4);
    for _ in 0..n {
        px.extend_from_slice(&[b, g, r, a]);
    }
    px
}

/// Composite and read the centre pixel of the display buffer as `(b, g, r, a)`.
fn composite_centre(canvas: &mut Canvas) -> (u8, u8, u8, u8) {
    canvas.present().unwrap();
    let px = canvas.read_display().unwrap();
    let idx = ((SIZE.height / 2 * SIZE.width) + SIZE.width / 2) as usize * 4;
    (px[idx], px[idx + 1], px[idx + 2], px[idx + 3])
}

/// A Normal top layer that is fully opaque hides the layer below it. This is
/// the control: it isolates stack ordering from any blend maths.
#[test]
#[ignore = "requires vulkan loader and device"]
fn opaque_normal_top_layer_hides_the_one_below() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("bottom", &filled(0, 0, 255, 255))
        .unwrap();
    let top = canvas
        .add_layer_with_pixels("top", &filled(255, 0, 0, 255))
        .unwrap();
    canvas
        .set_layer_blend(top, BlendMode::Normal, 1.0)
        .unwrap();

    let (b, _g, r, a) = composite_centre(&mut canvas);
    assert_eq!(a, 255, "composite should be opaque");
    assert!(b > 200, "top (blue) should win, got b={b}");
    assert!(r < 60, "bottom (red) should be hidden, got r={r}");
}

/// Regression: an opaque top layer set to Multiply must still sit on top.
///
/// Multiplying opaque blue over opaque red gives black-ish, and crucially it
/// must NOT leave the bottom layer's red showing through unchanged - that is
/// the "non-Normal layer sinks to the bottom" symptom.
#[test]
#[ignore = "requires vulkan loader and device"]
fn multiply_top_layer_stays_on_top() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("bottom", &filled(0, 0, 255, 255))
        .unwrap();
    let top = canvas
        .add_layer_with_pixels("top", &filled(255, 0, 0, 255))
        .unwrap();
    canvas
        .set_layer_blend(top, BlendMode::Multiply, 1.0)
        .unwrap();

    let (b, _g, r, a) = composite_centre(&mut canvas);
    assert_eq!(a, 255, "composite should be opaque");
    assert!(
        r < 200,
        "Multiply must darken the bottom layer's red; got r={r} \
         (unchanged red means the blended layer sank below the stack)"
    );
    assert!(b < 200, "Multiply of blue over red should not stay pure blue; got b={b}");
}

/// Screen lightens, Addition lightens at least as much, and both must differ
/// from Normal. Covers the two modes that also round-trip through sRGB.
#[test]
#[ignore = "requires vulkan loader and device"]
fn screen_and_addition_lighten() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("bottom", &filled(64, 64, 64, 255))
        .unwrap();
    let top = canvas
        .add_layer_with_pixels("top", &filled(96, 96, 96, 255))
        .unwrap();

    canvas.set_layer_blend(top, BlendMode::Normal, 1.0).unwrap();
    let normal = composite_centre(&mut canvas).0;
    canvas.set_layer_blend(top, BlendMode::Screen, 1.0).unwrap();
    let screen = composite_centre(&mut canvas).0;
    canvas
        .set_layer_blend(top, BlendMode::Addition, 1.0)
        .unwrap();
    let addition = composite_centre(&mut canvas).0;

    assert!(screen > normal, "Screen should lighten: {screen} vs {normal}");
    assert!(
        addition >= screen,
        "Addition should lighten at least as much as Screen: {addition} vs {screen}"
    );
}

/// Darken picks the darker channel. It is deliberately left in linear space -
/// `min` is monotonic, so converting to sRGB and back would be a no-op - and
/// this pins that the result really is the darker of the two inputs.
#[test]
#[ignore = "requires vulkan loader and device"]
fn darken_picks_the_darker_input() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("bottom", &filled(64, 64, 64, 255))
        .unwrap();
    let top = canvas
        .add_layer_with_pixels("top", &filled(192, 192, 192, 255))
        .unwrap();
    canvas.set_layer_blend(top, BlendMode::Darken, 1.0).unwrap();

    let (b, _g, _r, a) = composite_centre(&mut canvas);
    assert_eq!(a, 255);
    assert!(
        (58..=70).contains(&i32::from(b)),
        "Darken should keep the darker backdrop (~64), got {b}"
    );
}

/// The same ordering check for Overlay, the mode reported in the bug.
///
/// Uses neutral greys deliberately: Overlay is degenerate at the extremes
/// (`overlay(1, cs) == 1`, `overlay(0, cs) == 0`), so a saturated backdrop
/// would collapse to the backdrop for perfectly correct reasons and prove
/// nothing. Dark backdrop + light source gives three distinct outcomes -
/// backdrop-alone, Normal, and Overlay.
#[test]
#[ignore = "requires vulkan loader and device"]
fn overlay_top_layer_changes_the_result() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("bottom", &filled(64, 64, 64, 255))
        .unwrap();
    let top = canvas
        .add_layer_with_pixels("top", &filled(192, 192, 192, 255))
        .unwrap();

    canvas.set_layer_blend(top, BlendMode::Normal, 1.0).unwrap();
    let normal = composite_centre(&mut canvas);
    canvas
        .set_layer_blend(top, BlendMode::Overlay, 1.0)
        .unwrap();
    let overlay = composite_centre(&mut canvas);

    assert_ne!(
        normal, overlay,
        "switching to Overlay must change the composite"
    );
    // Backdrop is dark (< 0.5), so Overlay multiplies: result must be darker
    // than the Normal result, but must NOT collapse to the backdrop alone.
    assert!(
        overlay.0 < normal.0,
        "Overlay over a dark backdrop should darken: overlay={overlay:?} normal={normal:?}"
    );
    // Computed on gamma-encoded values (as other paint apps do), backdrop 64
    // over source 192 gives ~96. Computing it on linear values instead yields
    // ~66 - indistinguishable from the backdrop alone, which is the bug this
    // pins down.
    assert!(
        (80..=115).contains(&i32::from(overlay.0)),
        "Overlay should land near 96 (gamma-space); ~66 means it was computed \
         on linear values and collapsed to the backdrop. Got {overlay:?}"
    );
}
