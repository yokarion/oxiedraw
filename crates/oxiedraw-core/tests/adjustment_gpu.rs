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
#[ignore = "requires vulkan loader and device"]
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

/// The stroke-commit path (not just recompose) must respect folder scoping:
/// painting on a layer inside a folder with a brightness-0 adjustment must not
/// blacken a layer sitting below the folder.
#[test]
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
        .begin_transform_preview_gpu(target, &left_half_red(size), 64, 64)
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
#[ignore = "requires vulkan loader and device"]
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
        .begin_transform_preview_gpu(target, &solid(size, 0, 0, 255), 64, 64)
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

// Deleting a selection on an adjustment layer must refill the hole with white
// (full effect), not leave transparency. Mask slots stay opaque black-gray-white.
#[test]
#[ignore = "requires vulkan loader and device"]
fn delete_selection_refills_adjustment_mask_white() {
    let (mut canvas, adj) = adjustment_with_left_selection();
    canvas.erase_selection_in_layer(adj).unwrap();
    assert_mask_white(&mut canvas, adj);
}

// The cut path (clear_selection_from_layer) must also refill the hole white.
#[test]
#[ignore = "requires vulkan loader and device"]
fn cut_selection_refills_adjustment_mask_white() {
    let (mut canvas, adj) = adjustment_with_left_selection();
    canvas.clear_selection_from_layer(adj).unwrap();
    assert_mask_white(&mut canvas, adj);
}

// The selection-move lift (extract_selection_pixels) must also refill white.
#[test]
#[ignore = "requires vulkan loader and device"]
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
