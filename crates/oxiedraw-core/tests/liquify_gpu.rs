//! GPU integration tests for the Liquify tool, driven through the public
//! `Canvas` API. Each test is `#[ignore]` because it needs a working Vulkan
//! loader + device; run with `cargo test -p oxiedraw-core --test liquify_gpu
//! -- --ignored`.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::components::ComponentLibrary;
use oxiedraw_core::guides::{GuideConfig, Symmetry};
use oxiedraw_core::history::{HistoryAction, HistoryConfig, HistoryStack, LayerPatch};
use oxiedraw_core::liquify::{LiquifyMode, LiquifyStamp};
use oxiedraw_core::selection::{RectShape, SelectionShape};
use oxiedraw_core::tools::SelectionMode;
use oxiedraw_utils::geometry::{Point, Size};

const W: u32 = 128;
const H: u32 = 128;

fn size() -> Size {
    Size::new(W, H)
}

/// A canvas-sized BGRA8 buffer whose left half (x < `split`) is opaque red and
/// whose right half is opaque black. The vertical edge at `split` is what the
/// warp tests measure.
fn half_red(split: u32) -> Vec<u8> {
    let mut px = vec![0u8; (W * H) as usize * 4];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let red = x < split;
            px[i] = 0;
            px[i + 1] = 0;
            px[i + 2] = if red { 255 } else { 0 };
            px[i + 3] = 255;
        }
    }
    px
}

fn red_at(px: &[u8], x: u32, y: u32) -> u8 {
    px[((y * W + x) * 4 + 2) as usize]
}

/// First x along row `y` where the pixel stops being mostly red.
fn edge_x(px: &[u8], y: u32) -> u32 {
    (0..W).find(|&x| red_at(px, x, y) < 128).unwrap_or(W)
}

fn stamp(center: Point, drag: Point, radius: f32, mode: LiquifyMode) -> LiquifyStamp {
    LiquifyStamp {
        center,
        drag,
        radius,
        density: 0.5,
        strength: 0.5,
        mode,
    }
}

/// Drag horizontally from `from` to `to` at `y`, in dabs a fifth of a radius
/// apart - the same spacing the tool's gesture handler uses.
fn push_horizontal(canvas: &mut Canvas, y: f32, from: f32, to: f32, radius: f32) {
    let step = radius * 0.2;
    let steps = (((to - from).abs() / step).ceil() as usize).max(1);
    let dx = (to - from) / steps as f32;
    let stamps: Vec<LiquifyStamp> = (1..=steps)
        .map(|i| {
            stamp(
                Point::new(dx.mul_add(i as f32, from), y),
                Point::new(dx, 0.0),
                radius,
                LiquifyMode::ForwardWarp,
            )
        })
        .collect();
    canvas.liquify_stamps(&stamps).unwrap();
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn forward_warp_pushes_the_edge_along_the_drag() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();
    canvas.layers().set_active(Some(idx));

    canvas.begin_liquify(idx).unwrap();
    push_horizontal(&mut canvas, 64.0, 50.0, 90.0, 24.0);
    canvas.liquify_bake().unwrap();

    let out = canvas.read_layer(idx).unwrap();
    let moved = edge_x(&out, 64);
    assert!(moved > 70, "edge should have been pushed right, got x={moved}");
    // Rows far from the brush are outside its falloff and must be untouched.
    assert_eq!(edge_x(&out, 4), 64, "row far above the brush moved");
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn ending_without_baking_drops_only_the_pending_stroke() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let src = half_red(64);
    let idx = canvas.add_layer_with_pixels("t", &src).unwrap();

    canvas.begin_liquify(idx).unwrap();
    push_horizontal(&mut canvas, 64.0, 50.0, 90.0, 24.0);
    assert!(canvas.liquify_touched(), "the stroke should be pending a bake");
    canvas.end_liquify().unwrap();

    assert_eq!(canvas.read_layer(idx).unwrap(), src, "an unbaked stroke reached the layer");
    assert!(!canvas.liquify_active());
}

/// Restore All returns the layer to *exactly* the pixels the tool picked up.
/// Bit-exact equality is the point: it proves the snapshot stayed pristine
/// across the baked strokes rather than being re-taken from the warped layer,
/// which is what would make successive strokes stack resampling blur.
#[test]
#[ignore = "requires vulkan loader and device"]
fn restore_all_returns_to_the_pristine_pixels_bit_exact() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();
    let pristine = canvas.read_layer(idx).unwrap();

    canvas.begin_liquify(idx).unwrap();
    // Two separate baked strokes, as if the user lifted the pen between them.
    push_horizontal(&mut canvas, 64.0, 50.0, 90.0, 24.0);
    canvas.liquify_bake().unwrap();
    push_horizontal(&mut canvas, 40.0, 50.0, 20.0, 24.0);
    canvas.liquify_bake().unwrap();
    assert_ne!(canvas.read_layer(idx).unwrap(), pristine, "the warps did nothing");

    canvas.liquify_restore_all().unwrap();
    canvas.liquify_bake().unwrap();
    assert_eq!(
        canvas.read_layer(idx).unwrap(),
        pristine,
        "restore all did not return to the pristine pixels",
    );
}

/// Each stroke bakes on its own, so the layer advances once per stroke and the
/// UI has a distinct before/after to record per warp.
#[test]
#[ignore = "requires vulkan loader and device"]
fn each_stroke_bakes_separately() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();

    canvas.begin_liquify(idx).unwrap();
    push_horizontal(&mut canvas, 64.0, 50.0, 70.0, 24.0);
    canvas.liquify_bake().unwrap();
    assert!(!canvas.liquify_touched(), "bake left the stroke pending");
    let after_first = edge_x(&canvas.read_layer(idx).unwrap(), 64);

    push_horizontal(&mut canvas, 64.0, 70.0, 90.0, 24.0);
    assert!(canvas.liquify_touched(), "the second stroke is not pending");
    canvas.liquify_bake().unwrap();
    let after_second = edge_x(&canvas.read_layer(idx).unwrap(), 64);

    assert!(after_first > 64, "first stroke did not warp: {after_first}");
    assert!(
        after_second > after_first,
        "second stroke did not build on the first: {after_first} then {after_second}",
    );
}

/// The dirty bounds a bake reports must actually cover everything it changed -
/// the UI trusts them to size the history patch.
#[test]
#[ignore = "requires vulkan loader and device"]
fn dirty_bounds_cover_every_changed_pixel() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();

    canvas.begin_liquify(idx).unwrap();
    let before = canvas.read_layer(idx).unwrap();
    push_horizontal(&mut canvas, 64.0, 50.0, 90.0, 24.0);
    let (bx, by, bw, bh) = canvas.liquify_dirty_bounds().expect("a stroke is pending");
    canvas.liquify_bake().unwrap();
    let after = canvas.read_layer(idx).unwrap();

    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            if before[i..i + 4] == after[i..i + 4] {
                continue;
            }
            assert!(
                x >= bx && x < bx + bw && y >= by && y < by + bh,
                "pixel ({x}, {y}) changed outside the reported bounds \
                 ({bx}, {by}, {bw}, {bh})",
            );
        }
    }
}

/// Repeated pushes must compose, not merely add: dragging twice as far in two
/// strokes has to move the edge further than one stroke of the same length.
#[test]
#[ignore = "requires vulkan loader and device"]
fn successive_pushes_compose() {
    let one = {
        let mut canvas = Canvas::headless(size()).unwrap();
        let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();
        canvas.begin_liquify(idx).unwrap();
        push_horizontal(&mut canvas, 64.0, 50.0, 70.0, 24.0);
        canvas.liquify_bake().unwrap();
        edge_x(&canvas.read_layer(idx).unwrap(), 64)
    };
    let two = {
        let mut canvas = Canvas::headless(size()).unwrap();
        let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();
        canvas.begin_liquify(idx).unwrap();
        push_horizontal(&mut canvas, 64.0, 50.0, 70.0, 24.0);
        push_horizontal(&mut canvas, 64.0, 70.0, 90.0, 24.0);
        canvas.liquify_bake().unwrap();
        edge_x(&canvas.read_layer(idx).unwrap(), 64)
    };
    assert!(two > one, "second push did not build on the first: {one} then {two}");
}

/// The field update is a *composition*, `D_new(p) = d(p) + D_old(p + d(p))`,
/// not an addition `d(p) + D_old(p)`. The two only differ where the new dab
/// displaces a sample into a region the old field already covers, so this pins
/// exactly that: a first push that cannot reach the marker under addition still
/// changes where the marker lands under composition.
#[test]
#[ignore = "requires vulkan loader and device"]
fn composition_reads_the_old_field_at_the_displaced_position() {
    // A thin red marker bar at x in [46, 50) on black.
    let marker = {
        let mut px = vec![0u8; (W * H) as usize * 4];
        for y in 0..H {
            for x in 46..50 {
                let i = ((y * W + x) * 4) as usize;
                px[i + 2] = 255;
                px[i + 3] = 255;
            }
        }
        px
    };
    // Where the marker's left edge ends up, optionally after a far-left first
    // push that never touches the marker itself.
    let run = |with_first_push: bool| {
        let mut canvas = Canvas::headless(size()).unwrap();
        let idx = canvas.add_layer_with_pixels("t", &marker).unwrap();
        canvas.begin_liquify(idx).unwrap();
        if with_first_push {
            // Centred at x = 20, radius 20: covers [0, 40), well clear of the
            // marker at [46, 50).
            push_horizontal(&mut canvas, 64.0, 8.0, 32.0, 20.0);
        }
        // Centred around x = 70, radius 25: covers [45, 95), so it does reach
        // the marker and displaces its sample leftward into the first push's
        // region.
        push_horizontal(&mut canvas, 64.0, 58.0, 82.0, 25.0);
        canvas.liquify_bake().unwrap();
        let out = canvas.read_layer(idx).unwrap();
        (0..W).find(|&x| red_at(&out, x, 64) > 128)
    };

    let without = run(false).expect("marker survived without the first push");
    let with = run(true).expect("marker survived with the first push");
    // Under plain addition the first push contributes nothing at the marker
    // (its dab never covers those pixels), so the two would be identical.
    assert_ne!(
        with, without,
        "the first push did not reach the marker through the displaced sample, \
         so the field is being added rather than composed",
    );
}

/// The preview composite is rebuilt incrementally: only the first frame of a
/// session (and any frame after the below-stack cache is invalidated) redraws
/// the whole canvas, and later frames redraw just the region the warp touched.
///
/// That is the change most able to leave stale pixels on screen, and no other
/// test reaches it - they all read the preview once per session, which always
/// takes the full-rebuild path. This drives two frames and requires the
/// incremental result to be byte-identical to a single full rebuild of the same
/// field. The two strokes are far apart on purpose, so the second frame's clip
/// cannot cover the first stroke: those pixels have to survive from the earlier
/// frame rather than being redrawn.
#[test]
#[ignore = "requires vulkan loader and device"]
fn an_incrementally_updated_preview_matches_a_full_rebuild() {
    let stroke_a = |c: &mut Canvas| push_horizontal(c, 20.0, 50.0, 86.0, 18.0);
    let stroke_b = |c: &mut Canvas| push_horizontal(c, 108.0, 50.0, 20.0, 18.0);

    // Two preview frames: the second one takes the clipped path.
    let incremental = {
        let mut canvas = Canvas::headless(size()).unwrap();
        let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();
        canvas.layers().set_active(Some(idx));
        canvas.begin_liquify(idx).unwrap();
        stroke_a(&mut canvas);
        let _first_frame = canvas.read_liquify_preview().unwrap();
        stroke_b(&mut canvas);
        canvas.read_liquify_preview().unwrap()
    };

    // The same field, composited in one full rebuild.
    let full = {
        let mut canvas = Canvas::headless(size()).unwrap();
        let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();
        canvas.layers().set_active(Some(idx));
        canvas.begin_liquify(idx).unwrap();
        stroke_a(&mut canvas);
        stroke_b(&mut canvas);
        canvas.read_liquify_preview().unwrap()
    };

    assert_eq!(incremental.len(), full.len());
    let stale = incremental
        .chunks_exact(4)
        .zip(full.chunks_exact(4))
        .position(|(a, b)| a != b);
    assert!(
        stale.is_none(),
        "the incremental preview left a stale pixel at index {:?} \
         (canvas x={:?}, y={:?})",
        stale,
        stale.map(|i| i as u32 % W),
        stale.map(|i| i as u32 / W),
    );
}

/// The bake is clipped to the region the field changed. Anything else edited on
/// the layer during the session has to survive it - a full-canvas copy would
/// restore `warp(snapshot)` everywhere and silently revert that edit, without
/// it appearing in the bounded history patch either.
#[test]
#[ignore = "requires vulkan loader and device"]
fn baking_preserves_edits_made_outside_the_warped_region() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();

    canvas.begin_liquify(idx).unwrap();
    // Warp a small region near the top.
    push_horizontal(&mut canvas, 16.0, 50.0, 80.0, 20.0);

    // Meanwhile something else edits the layer far from the brush - the same
    // shape as pressing Delete with a selection, or applying a filter.
    let mut edited = canvas.read_layer(idx).unwrap();
    for y in 100..120 {
        for x in 10..30 {
            let i = ((y * W + x) * 4) as usize;
            edited[i] = 255; // blue
            edited[i + 2] = 0;
            edited[i + 3] = 255;
        }
    }
    canvas.restore_layer(idx, &edited).unwrap();

    canvas.liquify_bake().unwrap();

    let out = canvas.read_layer(idx).unwrap();
    let i = ((110 * W + 20) * 4) as usize;
    assert_eq!(
        &out[i..i + 4],
        &[255, 0, 0, 255],
        "the bake reverted an edit made outside the warped region",
    );
    // The warp itself still landed.
    assert!(edge_x(&out, 16) > 64, "the warp did not reach the layer");
}

/// The session pins a slot index, and the renderer follows it when the stack
/// shifts. Without that, inserting a layer underneath would make the next bake
/// write one layer's warp over another.
#[test]
#[ignore = "requires vulkan loader and device"]
fn the_session_follows_its_layer_through_a_stack_insert() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let bottom = canvas.add_layer_with_pixels("bottom", &half_red(64)).unwrap();
    let target = canvas.add_layer_with_pixels("target", &half_red(64)).unwrap();
    let bottom_before = canvas.read_layer(bottom).unwrap();

    canvas.begin_liquify(target).unwrap();
    assert_eq!(canvas.liquify_target(), Some(target));

    // Insert a layer at the bottom, pushing every index up by one.
    let inserted = canvas.add_layer("inserted").unwrap();
    canvas.reorder_layer(inserted, 0).unwrap();
    let target_now = canvas.liquify_target().expect("session survived the insert");
    assert_eq!(target_now, target + 1, "the session did not follow its layer");

    push_horizontal(&mut canvas, 64.0, 50.0, 90.0, 24.0);
    canvas.liquify_bake().unwrap();

    assert!(
        edge_x(&canvas.read_layer(target_now).unwrap(), 64) > 70,
        "the warp did not land on the tracked layer",
    );
    // Everything below shifted up by the same one slot (`headless` already has
    // a Background layer under these two).
    assert_eq!(
        canvas.read_layer(bottom + 1).unwrap(),
        bottom_before,
        "the warp landed on the wrong layer after the insert",
    );
}

/// The selection is the mask: pixels outside it are protected from every mode,
/// so a push that straddles the selection boundary only moves what is inside.
#[test]
#[ignore = "requires vulkan loader and device"]
fn a_selection_confines_the_warp_to_itself() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();

    // Select a horizontal band around y = 96; the rest of the canvas is masked.
    let shape = SelectionShape::Rect(RectShape {
        x: 0.0,
        y: 80.0,
        w: f32::from(u16::try_from(W).unwrap()),
        h: 32.0,
    });
    canvas.apply_selection_shape(&shape, SelectionMode::Replace).unwrap();
    assert!(canvas.selection_active());

    canvas.begin_liquify(idx).unwrap();
    // One push inside the band and one outside it, identical otherwise.
    push_horizontal(&mut canvas, 96.0, 50.0, 90.0, 24.0);
    push_horizontal(&mut canvas, 32.0, 50.0, 90.0, 24.0);
    canvas.liquify_bake().unwrap();

    let out = canvas.read_layer(idx).unwrap();
    assert!(
        edge_x(&out, 96) > 70,
        "the push inside the selection did not warp: {}",
        edge_x(&out, 96),
    );
    assert_eq!(
        edge_x(&out, 32),
        64,
        "the push outside the selection moved protected pixels",
    );
}

/// With a vertical mirror guide on the canvas centre, a rightward push on the
/// left half must produce a leftward push on the right half. This is the
/// property the `SymElement::linear` vector transform exists for - mirroring
/// only the dab centre would push both halves the same way.
#[test]
#[ignore = "requires vulkan loader and device"]
fn axis_symmetry_mirrors_the_push_direction() {
    let mut canvas = Canvas::headless(size()).unwrap();
    // Two vertical red bars, symmetric about x = 64: [24, 40) and [88, 104).
    let mut src = vec![0u8; (W * H) as usize * 4];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let bar = (24..40).contains(&x) || (88..104).contains(&x);
            src[i + 2] = if bar { 255 } else { 0 };
            src[i + 3] = 255;
        }
    }
    let idx = canvas.add_layer_with_pixels("t", &src).unwrap();

    let cfg = GuideConfig::centered(W, H);
    canvas.set_symmetry(Symmetry::from_config(&cfg));
    assert!(canvas.has_symmetry());

    canvas.begin_liquify(idx).unwrap();
    // Push the left bar to the right; the mirrored copy pushes the right bar
    // to the left by the same amount.
    push_horizontal(&mut canvas, 64.0, 30.0, 46.0, 20.0);
    canvas.liquify_bake().unwrap();

    let out = canvas.read_layer(idx).unwrap();
    let row = 64;
    // Right edge of the left bar, and left edge of the right bar.
    let left_bar_right = (0..W).rev().find(|&x| x < 64 && red_at(&out, x, row) > 128);
    let right_bar_left = (0..W).find(|&x| x > 64 && red_at(&out, x, row) > 128);
    let (l, r) = (left_bar_right.unwrap(), right_bar_left.unwrap());
    assert!(l > 39, "left bar was not pushed right (edge at {l})");
    assert!(r < 88, "right bar was not pushed left (edge at {r})");
    // Symmetric about the mirror line at x = 64.
    let (dl, dr) = (i64::from(64 - l), i64::from(r - 64));
    assert!(
        (dl - dr).abs() <= 2,
        "mirrored halves are not symmetric: {dl} vs {dr}",
    );
}

/// Pucker drags content toward the dab centre and Bloat pushes it away, so
/// with the dab sitting to the *right* of the red/black edge they move that
/// edge in opposite directions. The dab is deliberately off the edge: a radial
/// field has zero displacement at its own centre, so a dab centred exactly on
/// the edge leaves it where it is under both modes.
#[test]
#[ignore = "requires vulkan loader and device"]
fn bloat_and_pucker_move_content_in_opposite_directions() {
    let run = |mode: LiquifyMode| {
        let mut canvas = Canvas::headless(size()).unwrap();
        let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();
        canvas.begin_liquify(idx).unwrap();
        // Ten dabs on the spot, the way holding the pointer still would.
        let dabs: Vec<LiquifyStamp> = (0..10)
            .map(|_| stamp(Point::new(80.0, 64.0), Point::ZERO, 30.0, mode))
            .collect();
        canvas.liquify_stamps(&dabs).unwrap();
        canvas.liquify_bake().unwrap();
        edge_x(&canvas.read_layer(idx).unwrap(), 64)
    };
    let pucker = run(LiquifyMode::Pucker);
    let bloat = run(LiquifyMode::Bloat);
    assert!(pucker > 64, "pucker did not pull the edge toward the dab: {pucker}");
    assert!(bloat < 64, "bloat did not push the edge away from the dab: {bloat}");
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn reconstruct_eases_a_warp_back_out() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();

    canvas.begin_liquify(idx).unwrap();
    push_horizontal(&mut canvas, 64.0, 50.0, 90.0, 24.0);
    let warped = {
        let mut probe = canvas.read_liquify_preview().unwrap();
        probe.truncate((W * H) as usize * 4);
        edge_x(&probe, 64)
    };
    // Enough reconstruct dabs to take the field most of the way back to zero.
    let dabs: Vec<LiquifyStamp> = (0..20)
        .map(|_| {
            let mut s = stamp(
                Point::new(75.0, 64.0),
                Point::ZERO,
                40.0,
                LiquifyMode::Reconstruct,
            );
            s.strength = 1.0;
            s
        })
        .collect();
    canvas.liquify_stamps(&dabs).unwrap();
    canvas.liquify_bake().unwrap();

    let after = edge_x(&canvas.read_layer(idx).unwrap(), 64);
    assert!(warped > 70, "setup push did not warp: {warped}");
    assert!(after < warped, "reconstruct did not reduce the warp: {warped} -> {after}");
}

/// Bake + record one stroke exactly the way the tool's pen-up path does, and
/// return the state before and after it.
fn bake_stroke_into_history(
    canvas: &mut Canvas,
    history: &mut HistoryStack,
    idx: usize,
    layer_id: &str,
) -> (Vec<u8>, Vec<u8>) {
    let before = canvas.read_layer(idx).unwrap();
    canvas.liquify_bake().unwrap();
    let after = canvas.read_layer(idx).unwrap();
    if let Some(patch) = LayerPatch::from_full_diff(&before, &after, W, H) {
        history.record(HistoryAction::Liquify {
            layer_id: layer_id.to_string(),
            patch,
        });
    }
    (before, after)
}

/// Undo is granular *inside* the tool: each warp stroke is its own step, so
/// Ctrl+Z peels them back one at a time before ever reaching the brush stroke
/// underneath. Reproduces the reported bug, where the whole session stayed
/// unrecorded and the first Ctrl+Z popped the brush stroke instead.
#[test]
#[ignore = "requires vulkan loader and device"]
fn undo_steps_back_one_warp_stroke_at_a_time() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let mut history = HistoryStack::new(HistoryConfig::default());
    let mut components = ComponentLibrary::new();

    let idx = canvas.add_layer_with_pixels("t", &vec![0u8; (W * H) as usize * 4]).unwrap();
    let layer_id = canvas.layers().snapshot()[idx].id.clone();
    let empty = canvas.read_layer(idx).unwrap();

    // Stand-in for the earlier brush stroke: an ordinary recorded pixel edit.
    canvas.restore_layer(idx, &half_red(64)).unwrap();
    let painted = canvas.read_layer(idx).unwrap();
    history.record(HistoryAction::Stroke {
        layer_id: layer_id.clone(),
        patch: LayerPatch::from_full_diff(&empty, &painted, W, H).unwrap(),
    });

    // Two warp strokes in one session, each baked and recorded at pen-up.
    canvas.begin_liquify(idx).unwrap();
    push_horizontal(&mut canvas, 64.0, 50.0, 70.0, 24.0);
    let (_, after_first) = bake_stroke_into_history(&mut canvas, &mut history, idx, &layer_id);
    push_horizontal(&mut canvas, 64.0, 70.0, 90.0, 24.0);
    let (_, after_second) = bake_stroke_into_history(&mut canvas, &mut history, idx, &layer_id);
    assert_ne!(after_first, painted, "the first warp did nothing");
    assert_ne!(after_second, after_first, "the second warp did nothing");

    // Undo peels the strokes back one at a time, newest first.
    let label = history.undo(&mut canvas, &mut components).unwrap();
    assert_eq!(label.as_deref(), Some("Liquify"));
    assert_eq!(
        canvas.read_layer(idx).unwrap(),
        after_first,
        "the first undo skipped past the second warp stroke",
    );

    let label = history.undo(&mut canvas, &mut components).unwrap();
    assert_eq!(label.as_deref(), Some("Liquify"));
    assert_eq!(
        canvas.read_layer(idx).unwrap(),
        painted,
        "the second undo did not land on the pre-liquify pixels",
    );

    // Only now does undo reach the brush stroke underneath.
    let label = history.undo(&mut canvas, &mut components).unwrap();
    assert_eq!(label.as_deref(), Some("Brush stroke"));
    assert_eq!(canvas.read_layer(idx).unwrap(), empty);

    // Redo walks back up the same three steps.
    for _ in 0..3 {
        history.redo(&mut canvas, &mut components).unwrap();
    }
    assert_eq!(canvas.read_layer(idx).unwrap(), after_second);
}

/// A session the user opened but never warped bakes nothing, so there is no
/// empty history entry for Ctrl+Z to swallow.
#[test]
#[ignore = "requires vulkan loader and device"]
fn an_untouched_session_records_nothing() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let src = half_red(64);
    let idx = canvas.add_layer_with_pixels("t", &src).unwrap();

    canvas.begin_liquify(idx).unwrap();
    assert!(!canvas.liquify_touched());
    assert!(canvas.liquify_dirty_bounds().is_none());
    canvas.liquify_bake().unwrap();

    let after = canvas.read_layer(idx).unwrap();
    assert_eq!(after, src, "an untouched session altered the layer");
    assert!(
        LayerPatch::from_full_diff(&src, &after, W, H).is_none(),
        "an untouched session would have produced a history entry",
    );
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn commit_bakes_exactly_what_the_preview_showed() {
    let mut canvas = Canvas::headless(size()).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &half_red(64)).unwrap();
    canvas.layers().set_active(Some(idx));

    canvas.begin_liquify(idx).unwrap();
    push_horizontal(&mut canvas, 64.0, 50.0, 90.0, 24.0);
    let preview = canvas.read_liquify_preview().unwrap();
    canvas.liquify_bake().unwrap();
    let baked = canvas.read_layer(idx).unwrap();

    // The single opaque layer composites 1:1 into the preview, so the two must
    // agree pixel for pixel.
    let mut worst = 0i32;
    for (a, b) in preview.iter().zip(baked.iter()) {
        worst = worst.max((i32::from(*a) - i32::from(*b)).abs());
    }
    assert!(worst <= 2, "preview and commit diverged by {worst}");
}
