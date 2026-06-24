//! GPU integration tests for adjustment layers, driven through the public
//! `Canvas` API. Each test is `#[ignore]` because it needs a working Vulkan
//! loader + device; run with `cargo test -p oxiedraw-core --test adjustment_gpu
//! -- --ignored`.
//!
//! Adjustment layers filter the composited backdrop (everything below them), so
//! these read the composited canvas via `read_pixels`, not a single layer.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::Color;
use oxiedraw_core::effects::{AdjustmentData, Effect, EffectKind, StrokeSoftness};
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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

/// A red backdrop with a brightness-0 adjustment on top should composite to
/// black: the effect multiplies the whole backdrop down.
#[test]
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
/// colour, while the far corners stay transparent.
#[test]
#[ignore = "requires vulkan loader and device"]
fn stroke_colours_the_silhouette_edge() {
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
        "expected stroke colour just outside the edge, got B{b} G{g} R{r} A{a}"
    );

    // A far corner stays transparent (no edge nearby to stroke).
    let (_, _, _, corner_a) = at(0, 0);
    assert!(corner_a < 16, "far corner should stay transparent, got A{corner_a}");
}
