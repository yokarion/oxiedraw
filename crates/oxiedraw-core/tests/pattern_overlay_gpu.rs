//! GPU integration tests for the Pattern tool's overlay, driven through the
//! public `Canvas` API.
//!
//! The tool uploads its rasterised pattern as a premultiplied block and previews
//! it spliced in at the target layer's z-order; applying runs the same pass into
//! that layer. What these pin down is that the two agree. A preview that ignored
//! the layer's blend mode, its opacity, an adjustment above it or a clipping
//! mask would look right until the moment it was applied - which is the whole
//! reason the pattern goes through the composite rather than being painted over
//! the top of it.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::document::BlendMode;
use oxiedraw_core::effects::{AdjustmentData, Effect, EffectKind};
use oxiedraw_utils::geometry::Size;

const SIZE: Size = Size {
    width: 16,
    height: 16,
};

/// Where the pattern block is uploaded, and how big it is.
const BLOCK: (i32, i32, u32, u32) = (4, 4, 8, 8);

/// Full-canvas BGRA8 buffer where every pixel is `(b, g, r, a)`.
fn filled(b: u8, g: u8, r: u8, a: u8) -> Vec<u8> {
    let n = (SIZE.width * SIZE.height) as usize;
    (0..n).flat_map(|_| [b, g, r, a]).collect()
}

/// An opaque `w x h` premultiplied BGRA block - the shape a rasterised pattern
/// arrives in.
fn block(w: u32, h: u32, b: u8, g: u8, r: u8) -> Vec<u8> {
    (0..(w * h) as usize)
        .flat_map(|_| [b, g, r, 255])
        .collect()
}

/// Bytes where the two buffers differ by more than `tol`.
fn diff_bytes(a: &[u8], b: &[u8], tol: i32) -> usize {
    assert_eq!(a.len(), b.len(), "buffers are different sizes");
    a.iter()
        .zip(b)
        .filter(|(x, y)| (i32::from(**x) - i32::from(**y)).abs() > tol)
        .count()
}

/// Arm the overlay, read the live preview, then apply and read the committed
/// composite. Both are canvas-space BGRA8, so they compare byte for byte.
fn preview_then_apply(canvas: &mut Canvas, target: usize) -> (Vec<u8>, Vec<u8>) {
    let (x, y, w, h) = BLOCK;
    canvas
        .set_pattern_overlay(target, x, y, w, h, &block(w, h, 0, 255, 0))
        .unwrap();
    let preview = canvas.read_pattern_preview().unwrap();
    canvas.commit_pattern(target).unwrap();
    let applied = canvas.read_pixels().unwrap();
    (preview, applied)
}

/// Sanity check for the rest: the pattern really does land where it was put, so
/// a later "preview matches applied" is not two identical blank canvases.
#[test]
fn the_pattern_lands_where_it_was_uploaded() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let target = canvas
        .add_layer_with_pixels("layer", &filled(0, 0, 0, 0))
        .unwrap();

    let (_, applied) = preview_then_apply(&mut canvas, target);
    let at = |x: u32, y: u32| {
        let i = ((y * SIZE.width + x) * 4) as usize;
        [applied[i], applied[i + 1], applied[i + 2], applied[i + 3]]
    };
    assert_eq!(at(8, 8), [0, 255, 0, 255], "the block did not land");
    assert_eq!(at(1, 1), [0, 0, 0, 0], "the block leaked outside its rect");
    assert_eq!(at(14, 14), [0, 0, 0, 0], "the block leaked outside its rect");
}

/// A non-Normal blend mode and a partial opacity on the target layer have to
/// apply to the live pattern too, not only once it is baked in.
#[test]
fn preview_matches_the_applied_pixels_through_a_blend_mode() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("base", &filled(40, 90, 200, 255))
        .unwrap();
    let target = canvas
        .add_layer_with_pixels("pattern", &filled(0, 0, 0, 0))
        .unwrap();
    canvas
        .set_layer_blend(target, BlendMode::Multiply, 0.6)
        .unwrap();

    let (preview, applied) = preview_then_apply(&mut canvas, target);
    assert_eq!(
        diff_bytes(&preview, &applied, 1),
        0,
        "the live pattern did not composite through the layer's blend mode"
    );
}

/// An adjustment layer above the target filters the backdrop it sits on, the
/// live pattern included.
#[test]
fn preview_matches_the_applied_pixels_under_an_adjustment_layer() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    canvas
        .add_layer_with_pixels("base", &filled(30, 30, 30, 255))
        .unwrap();
    let target = canvas
        .add_layer_with_pixels("pattern", &filled(0, 0, 0, 0))
        .unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    canvas
        .set_layer_effects(
            adj,
            AdjustmentData {
                effects: vec![Effect::new(EffectKind::HueSatBright {
                    hue_degrees: 120.0,
                    saturation: 1.0,
                    brightness: 0.4,
                })],
            },
        )
        .unwrap();

    let (preview, applied) = preview_then_apply(&mut canvas, target);
    assert_eq!(
        diff_bytes(&preview, &applied, 1),
        0,
        "the live pattern was not run through the adjustment above it"
    );
}

/// A clipping mask confines the target layer to the alpha of the one below, and
/// the live pattern has to be confined with it - otherwise it covers the canvas
/// right up to the moment it is applied.
#[test]
fn preview_matches_the_applied_pixels_under_a_clipping_mask() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    // Base covers the left half only, so the clip has something to bite on.
    let mut base = vec![0_u8; (SIZE.width * SIZE.height) as usize * 4];
    for y in 0..SIZE.height {
        for x in 0..SIZE.width / 2 {
            let i = ((y * SIZE.width + x) * 4) as usize;
            base[i + 2] = 255;
            base[i + 3] = 255;
        }
    }
    canvas.add_layer_with_pixels("base", &base).unwrap();
    let target = canvas
        .add_layer_with_pixels("pattern", &filled(0, 0, 0, 0))
        .unwrap();
    canvas.set_layer_clipped(target, true).unwrap();

    let (preview, applied) = preview_then_apply(&mut canvas, target);
    assert_eq!(
        diff_bytes(&preview, &applied, 1),
        0,
        "the live pattern was not clipped to the base layer"
    );
    // And the clip really did cut it: the block spans both halves.
    let right = ((8 * SIZE.width + 11) * 4) as usize;
    assert_eq!(
        applied[right + 3], 0,
        "the clipped pattern should not reach past the base"
    );
}

/// Alpha lock recolours what is already there and adds nothing. The lock rides a
/// blend-state variant on the same pass the preview uses, so the preview shows
/// the constraint as well.
#[test]
fn an_alpha_locked_layer_keeps_its_alpha() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    // Paint on the left half only; the block straddles the boundary.
    let mut existing = vec![0_u8; (SIZE.width * SIZE.height) as usize * 4];
    for y in 0..SIZE.height {
        for x in 0..SIZE.width / 2 {
            let i = ((y * SIZE.width + x) * 4) as usize;
            existing[i + 2] = 255;
            existing[i + 3] = 255;
        }
    }
    let target = canvas
        .add_layer_with_pixels("pattern", &existing)
        .unwrap();
    canvas.set_layer_alpha_locked(target, true);

    let (preview, applied) = preview_then_apply(&mut canvas, target);
    assert_eq!(
        diff_bytes(&preview, &applied, 1),
        0,
        "the alpha-locked preview and the applied pixels disagree"
    );

    let layer = canvas.read_layer(target).unwrap();
    let at = |x: u32, y: u32| {
        let i = ((y * SIZE.width + x) * 4) as usize;
        [layer[i], layer[i + 1], layer[i + 2], layer[i + 3]]
    };
    assert_eq!(at(11, 8)[3], 0, "alpha lock let the pattern add coverage");
    assert_eq!(at(5, 8)[3], 255, "alpha lock ate coverage that was there");
    assert!(
        at(5, 8)[1] > 200,
        "the pattern should have recoloured the locked pixels, got {:?}",
        at(5, 8)
    );
}

/// Applying disarms the overlay, so a second Apply cannot lay the same pattern
/// down twice.
#[test]
fn applying_takes_the_overlay_off_screen() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let target = canvas
        .add_layer_with_pixels("layer", &filled(0, 0, 0, 0))
        .unwrap();
    let (x, y, w, h) = BLOCK;
    canvas
        .set_pattern_overlay(target, x, y, w, h, &block(w, h, 0, 255, 0))
        .unwrap();
    assert!(canvas.pattern_overlay_active());

    canvas.commit_pattern(target).unwrap();
    assert!(!canvas.pattern_overlay_active(), "the overlay stayed armed");
    assert!(
        canvas.commit_pattern(target).is_err(),
        "a disarmed overlay must refuse to apply again"
    );
}

/// Cancelling leaves the layer exactly as it was.
#[test]
fn cancelling_leaves_the_layer_alone() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let target = canvas
        .add_layer_with_pixels("layer", &filled(10, 20, 30, 255))
        .unwrap();
    let before = canvas.read_layer(target).unwrap();

    let (x, y, w, h) = BLOCK;
    canvas
        .set_pattern_overlay(target, x, y, w, h, &block(w, h, 0, 255, 0))
        .unwrap();
    canvas.cancel_pattern_overlay();

    assert!(!canvas.pattern_overlay_active());
    assert_eq!(
        canvas.read_layer(target).unwrap(),
        before,
        "cancelling wrote to the layer"
    );
}
