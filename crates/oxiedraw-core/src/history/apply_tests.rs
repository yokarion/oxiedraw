//! Undo/redo round-trip tests - one per [`HistoryAction`] variant.
//!
//! Each test drives a real [`Canvas`] (so they require a Vulkan loader +
//! device and are `#[ignore]`d by default, matching the rest of the GPU
//! suite). The pattern is always the same:
//!
//! 1. Set up a known *before* state.
//! 2. Perform the real mutation to reach the *after* state and record the
//!    matching [`HistoryAction`].
//! 3. `undo` -> assert the canvas matches *before*.
//! 4. `redo` -> assert the canvas matches *after*.

use oxiedraw_utils::geometry::Size;

use crate::canvas::Canvas;
use crate::components::ComponentLibrary;
use crate::history::{
    CropLayer, FoldedLayer, HistoryAction, HistoryConfig, HistoryStack, LayerPatch,
    SelectionSnapshot, capture_layer,
};
use crate::tools::{CropRect, SelectionMode};
use crate::selection::SelectionShape;

const W: u32 = 16;
const H: u32 = 16;

fn canvas() -> Canvas {
    Canvas::headless(Size::new(W, H)).expect("headless canvas init")
}

fn stack() -> HistoryStack {
    HistoryStack::new(HistoryConfig::default())
}

fn layer_id(c: &Canvas, idx: usize) -> String {
    c.layers().snapshot()[idx].id.clone()
}

fn layer_ids(c: &Canvas) -> Vec<String> {
    c.layers().snapshot().iter().map(|l| l.id.clone()).collect()
}

/// Solid-fill a layer and return the resulting (read-back) pixels so callers
/// compare against what the GPU actually stored, not what we asked for.
fn paint(c: &mut Canvas, idx: usize, color: [f32; 4]) -> Vec<u8> {
    c.clear_layer_at(idx, color).expect("clear_layer_at");
    c.read_layer(idx).expect("read_layer")
}

// ---------------------------------------------------------------------------
// Pixel patch variants: Stroke / Fill / Clear / Transform share one apply path
// ---------------------------------------------------------------------------

/// Helper: round-trip a single `{layer_id, patch}` action built from a
/// transparent -> red diff on layer 0.
fn patch_round_trip(make: impl Fn(String, LayerPatch) -> HistoryAction) {
    let mut c = canvas();
    let mut s = stack();
    let id = layer_id(&c, 0);

    let before = c.read_layer(0).expect("before");
    let after = paint(&mut c, 0, [1.0, 0.0, 0.0, 1.0]);
    assert_ne!(before, after, "fill must change pixels");

    let patch = LayerPatch::from_full_diff(&before, &after, W, H).expect("non-empty patch");
    s.record(make(id, patch));

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(c.read_layer(0).expect("after undo"), before);

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!(c.read_layer(0).expect("after redo"), after);
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn stroke_round_trip() {
    patch_round_trip(|layer_id, patch| HistoryAction::Stroke { layer_id, patch });
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn fill_round_trip() {
    patch_round_trip(|layer_id, patch| HistoryAction::Fill { layer_id, patch });
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn clear_round_trip() {
    patch_round_trip(|layer_id, patch| HistoryAction::Clear { layer_id, patch });
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn transform_round_trip() {
    patch_round_trip(|layer_id, patch| HistoryAction::Transform { layer_id, patch });
}

// ---------------------------------------------------------------------------
// Layer-structure variants
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_add_round_trip() {
    let mut c = canvas();
    let mut s = stack();

    // Reach the after-state: a new painted layer on top.
    let new_idx = c.add_layer("Added").expect("add_layer");
    let pixels = paint(&mut c, new_idx, [0.0, 1.0, 0.0, 1.0]);
    let (id, name, visible, _, _, _, _) = capture_layer(&mut c, new_idx).expect("capture");
    s.record(HistoryAction::LayerAdd { idx: new_idx, id: id.clone(), name, visible, layer_kind: crate::document::LayerKind::Raster, blend: crate::document::BlendMode::Normal, opacity: 1.0, pixels: pixels.clone() });
    assert_eq!(c.layers().len(), 2);

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(c.layers().len(), 1);
    assert!(!layer_ids(&c).contains(&id));

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!(c.layers().len(), 2);
    let restored = layer_ids(&c).iter().position(|x| x == &id).expect("layer back");
    assert_eq!(c.read_layer(restored).expect("pixels"), pixels);
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_remove_round_trip() {
    let mut c = canvas();
    let mut s = stack();
    let extra = c.add_layer("Doomed").expect("add_layer");
    let pixels = paint(&mut c, extra, [0.0, 0.0, 1.0, 1.0]);
    let (id, name, visible, _, _, _, _) = capture_layer(&mut c, extra).expect("capture");

    c.remove_layer(extra).expect("remove_layer");
    s.record(HistoryAction::LayerRemove { idx: extra, id: id.clone(), name, visible, layer_kind: crate::document::LayerKind::Raster, blend: crate::document::BlendMode::Normal, opacity: 1.0, pixels: pixels.clone() });
    assert_eq!(c.layers().len(), 1);

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(c.layers().len(), 2);
    let back = layer_ids(&c).iter().position(|x| x == &id).expect("layer back");
    assert_eq!(c.read_layer(back).expect("pixels"), pixels);

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!(c.layers().len(), 1);
    assert!(!layer_ids(&c).contains(&id));
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_blend_round_trip() {
    use crate::document::BlendMode;
    let mut c = canvas();
    let mut s = stack();
    let id = layer_id(&c, 0);
    assert_eq!(c.layers().blend(0), Some((BlendMode::Normal, 1.0)));

    c.set_layer_blend(0, BlendMode::Multiply, 0.5).expect("set blend");
    s.record(HistoryAction::LayerBlend {
        id,
        old_blend: BlendMode::Normal,
        old_opacity: 1.0,
        new_blend: BlendMode::Multiply,
        new_opacity: 0.5,
    });
    assert_eq!(c.layers().blend(0), Some((BlendMode::Multiply, 0.5)));

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(c.layers().blend(0), Some((BlendMode::Normal, 1.0)));

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!(c.layers().blend(0), Some((BlendMode::Multiply, 0.5)));
}

/// Center-pixel BGRA bytes of the composited canvas.
fn canvas_center_bgra(c: &mut Canvas) -> [u8; 4] {
    let px = c.read_pixels().expect("read_pixels");
    let i = ((H / 2 * W + W / 2) * 4) as usize;
    [px[i], px[i + 1], px[i + 2], px[i + 3]]
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn multiply_blend_darkens_canvas() {
    use crate::document::BlendMode;
    let mut c = canvas();
    paint(&mut c, 0, [0.5, 0.5, 0.5, 1.0]); // bottom: mid grey
    let top = c.add_layer("Top").expect("add");
    paint(&mut c, top, [0.4, 0.6, 0.8, 1.0]);

    // Normal opaque: canvas shows the top layer's colour.
    c.set_layer_blend(top, BlendMode::Normal, 1.0).expect("normal");
    let normal = canvas_center_bgra(&mut c);

    // Multiply darkens: every channel must drop versus Normal (top * bottom
    // < top since bottom < 1.0).
    c.set_layer_blend(top, BlendMode::Multiply, 1.0).expect("multiply");
    let multiplied = canvas_center_bgra(&mut c);
    for ch in 0..3 {
        assert!(
            multiplied[ch] < normal[ch],
            "multiply channel {ch}: {} !< {}",
            multiplied[ch],
            normal[ch]
        );
    }
    assert_eq!(multiplied[3], 255, "opaque stack stays opaque");
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn zero_opacity_top_layer_is_invisible() {
    use crate::document::BlendMode;
    let mut c = canvas();
    paint(&mut c, 0, [0.5, 0.5, 0.5, 1.0]);
    let bottom_only = canvas_center_bgra(&mut c);

    let top = c.add_layer("Top").expect("add");
    paint(&mut c, top, [0.9, 0.1, 0.2, 1.0]);
    c.set_layer_blend(top, BlendMode::Normal, 0.0).expect("transparent");

    assert_eq!(
        canvas_center_bgra(&mut c),
        bottom_only,
        "a 0%-opacity top layer must not change the canvas"
    );
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn transform_preview_identity_matches_committed_blend() {
    use crate::document::BlendMode;
    use oxiedraw_utils::geometry::TransformRect;

    let mut c = canvas();
    paint(&mut c, 0, [0.5, 0.5, 0.5, 1.0]); // bottom: mid grey
    let top = c.add_layer("Top").expect("add");
    paint(&mut c, top, [0.4, 0.6, 0.8, 1.0]);
    c.set_layer_blend(top, BlendMode::Multiply, 1.0).expect("multiply");

    // The committed canvas (top Multiply over grey) is the reference.
    let expected = canvas_center_bgra(&mut c);

    // Drive the live preview with an identity transform of the top layer: the
    // warped layer equals its original pixels, so the preview must reproduce
    // the committed Multiply composite.
    let src = c.read_layer(top).expect("read top");
    c.clear_layer_at(top, [0.0, 0.0, 0.0, 0.0]).expect("clear top");
    let mut above = Vec::new();
    c.begin_transform_preview(top, &mut above).expect("begin preview base");
    c.begin_transform_preview_gpu(top, &src, W, H).expect("begin gpu preview");
    #[allow(clippy::cast_precision_loss)]
    let rect = TransformRect::new(W as f32 / 2.0, H as f32 / 2.0, W as f32, H as f32, 0.0);
    c.set_transform_preview(rect, rect, W, H);

    let preview = c.read_transform_preview().expect("read preview");
    let i = ((H / 2 * W + W / 2) * 4) as usize;
    let got = [preview[i], preview[i + 1], preview[i + 2], preview[i + 3]];
    for ch in 0..4 {
        let d = i32::from(got[ch]).abs_diff(i32::from(expected[ch]));
        assert!(
            d <= 2,
            "channel {ch}: preview {} vs committed {} (diff {d})",
            got[ch],
            expected[ch]
        );
    }
    c.clear_transform_preview();
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn transform_preview_keeps_layers_below() {
    use crate::document::BlendMode;
    use oxiedraw_utils::geometry::TransformRect;

    let mut c = canvas();
    paint(&mut c, 0, [0.9, 0.1, 0.1, 1.0]); // bottom: red
    let top = c.add_layer("Top").expect("add");
    paint(&mut c, top, [0.1, 0.1, 0.9, 1.0]); // top: blue
    let bottom_committed = {
        c.set_layer_blend(top, BlendMode::Normal, 0.0).expect("hide top");
        let p = canvas_center_bgra(&mut c);
        c.set_layer_blend(top, BlendMode::Normal, 1.0).expect("show top");
        p
    };

    // Transform the top layer and translate it far off-canvas: the center pixel
    // is no longer covered by the (warped) top, so the preview there must show
    // the bottom layer - not a hole.
    let src = c.read_layer(top).expect("read top");
    c.clear_layer_at(top, [0.0, 0.0, 0.0, 0.0]).expect("clear top");
    let mut above = Vec::new();
    c.begin_transform_preview(top, &mut above).expect("begin base");
    c.begin_transform_preview_gpu(top, &src, W, H).expect("begin gpu");
    #[allow(clippy::cast_precision_loss)]
    let orig = TransformRect::new(W as f32 / 2.0, H as f32 / 2.0, W as f32, H as f32, 0.0);
    #[allow(clippy::cast_precision_loss)]
    let moved = TransformRect::new(W as f32 * 4.0, H as f32 / 2.0, W as f32, H as f32, 0.0);
    c.set_transform_preview(orig, moved, W, H);

    let preview = c.read_transform_preview().expect("read preview");
    let i = ((H / 2 * W + W / 2) * 4) as usize;
    let got = [preview[i], preview[i + 1], preview[i + 2], preview[i + 3]];
    assert_eq!(
        got, bottom_committed,
        "center must show the bottom layer where the top moved away"
    );
    c.clear_transform_preview();
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn transform_preview_partial_layer_tight_bounds() {
    use crate::document::BlendMode;
    use oxiedraw_utils::geometry::TransformRect;

    let mut c = canvas();
    paint(&mut c, 0, [0.9, 0.1, 0.1, 1.0]); // bottom: red, fills canvas
    let top = c.add_layer("Top").expect("add");
    // Top: a single opaque green pixel near a corner, rest transparent (so the
    // tight content bounds are far smaller than the canvas - the real app case).
    let mut top_px = vec![0u8; (W * H * 4) as usize];
    let pi = ((2 * W + 3) * 4) as usize; // pixel (3,2)
    top_px[pi] = 0; // B
    top_px[pi + 1] = 255; // G
    top_px[pi + 2] = 0; // R
    top_px[pi + 3] = 255; // A
    c.restore_layer(top, &top_px).expect("write top");
    c.set_layer_blend(top, BlendMode::Normal, 1.0).expect("normal");
    let committed = c.read_pixels().expect("committed");

    // Identity transform with the tight content bounds the UI would compute
    // (the single pixel at (3,2) -> a 1x1 box centred at (3.5, 2.5)).
    let src = c.read_layer(top).expect("read top");
    let orig = TransformRect::new(3.5, 2.5, 1.0, 1.0, 0.0);
    c.clear_layer_at(top, [0.0, 0.0, 0.0, 0.0]).expect("clear top");
    let mut above = Vec::new();
    c.begin_transform_preview(top, &mut above).expect("begin base");
    c.begin_transform_preview_gpu(top, &src, W, H).expect("begin gpu");
    c.set_transform_preview(orig, orig, W, H);

    let preview = c.read_transform_preview().expect("read preview");
    assert_eq!(
        preview, committed,
        "identity preview of a partial layer must equal the committed canvas"
    );
    c.clear_transform_preview();
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn transform_preview_bottom_layer_visible() {
    use crate::document::BlendMode;
    use oxiedraw_utils::geometry::TransformRect;

    let mut c = canvas();
    // Single layer (the bottom/background), index 0.
    paint(&mut c, 0, [0.2, 0.7, 0.3, 1.0]); // green
    c.set_layer_blend(0, BlendMode::Normal, 1.0).expect("normal");
    let committed = canvas_center_bgra(&mut c);

    let src = c.read_layer(0).expect("read");
    c.clear_layer_at(0, [0.0, 0.0, 0.0, 0.0]).expect("clear");
    let mut above = Vec::new();
    c.begin_transform_preview(0, &mut above).expect("begin base");
    c.begin_transform_preview_gpu(0, &src, W, H).expect("begin gpu");
    #[allow(clippy::cast_precision_loss)]
    let rect = TransformRect::new(W as f32 / 2.0, H as f32 / 2.0, W as f32, H as f32, 0.0);
    c.set_transform_preview(rect, rect, W, H); // identity (at rest)

    // At rest, the (only) layer must still be visible in the preview.
    let preview = c.read_transform_preview().expect("read preview");
    let i = ((H / 2 * W + W / 2) * 4) as usize;
    let got = [preview[i], preview[i + 1], preview[i + 2], preview[i + 3]];
    assert_eq!(got, committed, "the bottom layer must show at rest in the preview");
    c.clear_transform_preview();
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn transform_preview_warped_layer_moves() {
    use crate::document::BlendMode;
    use oxiedraw_utils::geometry::TransformRect;

    let mut c = canvas();
    paint(&mut c, 0, [0.9, 0.1, 0.1, 1.0]); // bottom: red
    let top = c.add_layer("Top").expect("add");
    // Top: a 4x4 opaque green block in the top-left corner.
    let mut top_px = vec![0u8; (W * H * 4) as usize];
    for y in 0..4u32 {
        for x in 0..4u32 {
            let p = ((y * W + x) * 4) as usize;
            top_px[p + 1] = 255; // G
            top_px[p + 3] = 255; // A
        }
    }
    c.restore_layer(top, &top_px).expect("write top");
    c.set_layer_blend(top, BlendMode::Normal, 1.0).expect("normal");
    // Reference colour of the bottom layer at a pixel the top never covers.
    let red_below = {
        c.set_layer_blend(top, BlendMode::Normal, 0.0).expect("hide");
        let p = canvas_center_bgra(&mut c);
        c.set_layer_blend(top, BlendMode::Normal, 1.0).expect("show");
        p
    };

    let src = c.read_layer(top).expect("read top");
    c.clear_layer_at(top, [0.0, 0.0, 0.0, 0.0]).expect("clear top");
    let mut above = Vec::new();
    c.begin_transform_preview(top, &mut above).expect("begin base");
    c.begin_transform_preview_gpu(top, &src, W, H).expect("begin gpu");
    // Tight bounds of the block (a 4x4 box centred at (2,2)), translated +6 in x.
    let orig = TransformRect::new(2.0, 2.0, 4.0, 4.0, 0.0);
    let moved = TransformRect::new(8.0, 2.0, 4.0, 4.0, 0.0);
    c.set_transform_preview(orig, moved, W, H);

    let preview = c.read_transform_preview().expect("read preview");
    let at = |x: u32, y: u32| {
        let i = ((y * W + x) * 4) as usize;
        [preview[i], preview[i + 1], preview[i + 2], preview[i + 3]]
    };
    // (8,2) is inside the moved block -> green; (2,2) is where it left -> red.
    let moved_px = at(8, 2);
    assert!(
        moved_px[1] > 180 && moved_px[2] < 80,
        "warped layer must appear at its moved position, got {moved_px:?}"
    );
    assert_eq!(
        at(2, 2),
        red_below,
        "the bottom layer must show where the top moved away"
    );
    c.clear_transform_preview();
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_reorder_round_trip() {
    let mut c = canvas();
    let mut s = stack();
    c.add_layer("Middle").expect("add");
    c.add_layer("Top").expect("add");
    let before = layer_ids(&c);

    c.reorder_layer(0, 2).expect("reorder");
    let after = layer_ids(&c);
    assert_ne!(before, after);
    s.record(HistoryAction::LayerReorder { from: 0, to: 2 });

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(layer_ids(&c), before);

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!(layer_ids(&c), after);
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_rename_round_trip() {
    let mut c = canvas();
    let mut s = stack();
    let id = layer_id(&c, 0);

    c.layers().rename(0, "Renamed");
    s.record(HistoryAction::LayerRename {
        id,
        old_name: "Background".to_string(),
        new_name: "Renamed".to_string(),
    });
    assert_eq!(c.layers().snapshot()[0].name, "Renamed");

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(c.layers().snapshot()[0].name, "Background");

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!(c.layers().snapshot()[0].name, "Renamed");
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_visibility_round_trip() {
    let mut c = canvas();
    let mut s = stack();
    let id = layer_id(&c, 0);
    assert!(c.layers().snapshot()[0].visible);

    c.set_layer_visible(0, false).expect("hide");
    s.record(HistoryAction::LayerVisibility { id, old: true, new: false });
    assert!(!c.layers().snapshot()[0].visible);

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert!(c.layers().snapshot()[0].visible);

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert!(!c.layers().snapshot()[0].visible);
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_duplicate_round_trip() {
    let mut c = canvas();
    let mut s = stack();
    let src_pixels = paint(&mut c, 0, [1.0, 1.0, 0.0, 1.0]);

    let new_idx = c.duplicate_layer(0).expect("duplicate");
    let (new_id, new_name, _, _, _, _, _) = capture_layer(&mut c, new_idx).expect("capture");
    let dup_pixels = c.read_layer(new_idx).expect("dup pixels");
    assert_eq!(dup_pixels, src_pixels, "duplicate copies pixels");
    s.record(HistoryAction::LayerDuplicate {
        src_idx: 0,
        new_idx,
        new_id: new_id.clone(),
        new_name,
        layer_kind: crate::document::LayerKind::Raster,
        blend: crate::document::BlendMode::Normal,
        opacity: 1.0,
        pixels: dup_pixels.clone(),
    });
    assert_eq!(c.layers().len(), 2);

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(c.layers().len(), 1);
    assert!(!layer_ids(&c).contains(&new_id));

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!(c.layers().len(), 2);
    let back = layer_ids(&c).iter().position(|x| x == &new_id).expect("dup back");
    assert_eq!(c.read_layer(back).expect("pixels"), dup_pixels);
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_duplicate_inherits_blend() {
    use crate::document::BlendMode;
    let mut c = canvas();
    paint(&mut c, 0, [1.0, 1.0, 0.0, 1.0]);
    c.set_layer_blend(0, BlendMode::Multiply, 0.5).expect("set blend");

    let new_idx = c.duplicate_layer(0).expect("duplicate");
    assert_eq!(
        c.layers().blend(new_idx),
        Some((BlendMode::Multiply, 0.5)),
        "duplicate inherits the source blend mode + opacity"
    );
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_duplicate_blend_survives_redo() {
    use crate::document::BlendMode;
    let mut c = canvas();
    let mut s = stack();
    let src_pixels = paint(&mut c, 0, [1.0, 1.0, 0.0, 1.0]);
    c.set_layer_blend(0, BlendMode::Screen, 0.4).expect("set blend");

    let new_idx = c.duplicate_layer(0).expect("duplicate");
    let (new_id, new_name, _, _, blend, opacity, dup_pixels) =
        capture_layer(&mut c, new_idx).expect("capture");
    assert_eq!((blend, opacity), (BlendMode::Screen, 0.4));
    s.record(HistoryAction::LayerDuplicate {
        src_idx: 0,
        new_idx,
        new_id: new_id.clone(),
        new_name,
        layer_kind: crate::document::LayerKind::Raster,
        blend,
        opacity,
        pixels: dup_pixels,
    });

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(c.layers().len(), 1);

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    let back = layer_ids(&c).iter().position(|x| x == &new_id).expect("dup back");
    assert_eq!(c.read_layer(back).expect("pixels"), src_pixels);
    assert_eq!(
        c.layers().blend(back),
        Some((BlendMode::Screen, 0.4)),
        "redo restores the duplicate's blend mode + opacity"
    );
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_merge_bakes_blend() {
    use crate::document::BlendMode;
    let mut c = canvas();
    paint(&mut c, 0, [0.5, 0.5, 0.5, 1.0]); // bottom: mid grey
    let top = c.add_layer("Top").expect("add");
    paint(&mut c, top, [0.4, 0.6, 0.8, 1.0]);
    c.set_layer_blend(top, BlendMode::Multiply, 1.0).expect("multiply");

    // The composited (multiplied) look before merging.
    let before = canvas_center_bgra(&mut c);

    let survivor = c.merge_layers(&[0, top]).expect("merge");
    assert_eq!(c.layers().len(), 1);
    // The survivor flattens to Normal/opaque (blend is baked into the pixels).
    assert_eq!(
        c.layers().blend(survivor),
        Some((BlendMode::Normal, 1.0)),
        "merged survivor resets to Normal so it is not blended twice"
    );
    // The merged canvas matches what the multiply looked like, not a plain stack.
    let after = canvas_center_bgra(&mut c);
    for ch in 0..4 {
        assert!(
            (i16::from(after[ch]) - i16::from(before[ch])).abs() <= 1,
            "merged channel {ch}: {} vs pre-merge {}",
            after[ch],
            before[ch]
        );
    }
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn layer_merge_round_trip() {
    let mut c = canvas();
    let mut s = stack();
    c.add_layer("Top").expect("add");
    // Distinct pixels per layer so the merge result differs from either input.
    let bg_pixels = paint(&mut c, 0, [1.0, 0.0, 0.0, 1.0]);
    let top_pixels = paint(&mut c, 1, [0.0, 0.0, 1.0, 0.5]);

    let survivor_pre = c.read_layer(0).expect("survivor pre");
    assert_eq!(survivor_pre, bg_pixels);
    let folded = vec![FoldedLayer {
        idx: 1,
        id: layer_id(&c, 1),
        name: c.layers().snapshot()[1].name.clone(),
        visible: c.layers().snapshot()[1].visible,
        blend: crate::document::BlendMode::Normal,
        opacity: 1.0,
        pixels: top_pixels.clone(),
    }];

    c.merge_layers(&[0, 1]).expect("merge");
    let survivor_post = c.read_layer(0).expect("survivor post");
    assert_eq!(c.layers().len(), 1);
    s.record(HistoryAction::LayerMerge {
        survivor_idx: 0,
        survivor_pre,
        survivor_post: survivor_post.clone(),
        survivor_blend: crate::document::BlendMode::Normal,
        survivor_opacity: 1.0,
        folded,
    });

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(c.layers().len(), 2);
    assert_eq!(c.read_layer(0).expect("bg restored"), bg_pixels);
    assert_eq!(c.read_layer(1).expect("top restored"), top_pixels);

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!(c.layers().len(), 1);
    assert_eq!(c.read_layer(0).expect("survivor"), survivor_post);
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires vulkan loader and device"]
fn selection_change_round_trip() {
    let mut c = canvas();
    let mut s = stack();
    assert!(!c.selection_active());

    // before: no selection. after: select-all mask.
    c.select_all().expect("select_all");
    assert!(c.selection_active());
    let after_mask = c.read_selection_mask().expect("mask");
    s.record(HistoryAction::SelectionChange {
        before: SelectionSnapshot { active: false, mask: None },
        after: SelectionSnapshot { active: true, mask: Some(after_mask.clone()) },
    });

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert!(!c.selection_active());

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert!(c.selection_active());
    assert_eq!(c.read_selection_mask().expect("mask back"), after_mask);
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn selection_change_mask_round_trip() {
    let mut c = canvas();
    let mut s = stack();

    // before: a rect selection. after: a different (smaller) rect.
    let rect_a = crate::selection::RectShape { x: 0.0, y: 0.0, w: 8.0, h: 8.0 };
    c.apply_selection_shape(&SelectionShape::Rect(rect_a), SelectionMode::Replace).expect("sel a");
    let mask_a = c.read_selection_mask().expect("mask a");

    let rect_b = crate::selection::RectShape { x: 4.0, y: 4.0, w: 4.0, h: 4.0 };
    c.apply_selection_shape(&SelectionShape::Rect(rect_b), SelectionMode::Replace).expect("sel b");
    let mask_b = c.read_selection_mask().expect("mask b");
    assert_ne!(mask_a, mask_b);

    s.record(HistoryAction::SelectionChange {
        before: SelectionSnapshot { active: true, mask: Some(mask_a.clone()) },
        after: SelectionSnapshot { active: true, mask: Some(mask_b.clone()) },
    });

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(c.read_selection_mask().expect("mask after undo"), mask_a);

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!(c.read_selection_mask().expect("mask after redo"), mask_b);
}

// ---------------------------------------------------------------------------
// Crop
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires vulkan loader and device"]
fn crop_canvas_round_trip() {
    let mut c = canvas();
    let mut s = stack();
    paint(&mut c, 0, [0.3, 0.6, 0.9, 1.0]);

    let before_size = (c.size().width, c.size().height);
    let before_layers: Vec<CropLayer> = (0..c.layers().len())
        .filter_map(|i| {
            let (id, name, visible, kind, blend, opacity, pixels) = capture_layer(&mut c, i)?;
            Some(CropLayer {
                id,
                name,
                visible,
                pixels,
                kind,
                blend,
                opacity,
            })
        })
        .collect();

    // Crop to the top-left 8x8 quadrant.
    c.apply_crop(CropRect::new(0.0, 0.0, 8.0, 8.0)).expect("crop");
    let after_size = (c.size().width, c.size().height);
    assert_eq!(after_size, (8, 8));
    let after_layers: Vec<CropLayer> = (0..c.layers().len())
        .filter_map(|i| {
            let (id, name, visible, kind, blend, opacity, pixels) = capture_layer(&mut c, i)?;
            Some(CropLayer {
                id,
                name,
                visible,
                pixels,
                kind,
                blend,
                opacity,
            })
        })
        .collect();

    s.record(HistoryAction::CropCanvas {
        before_size,
        after_size,
        before_layers: before_layers.clone(),
        after_layers: after_layers.clone(),
        active_layer: c.layers().active(),
    });

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!((c.size().width, c.size().height), before_size);
    assert_eq!(c.read_layer(0).expect("pixels"), before_layers[0].pixels);

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!((c.size().width, c.size().height), after_size);
    assert_eq!(c.read_layer(0).expect("pixels"), after_layers[0].pixels);
}

// ---------------------------------------------------------------------------
// Batch
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires vulkan loader and device"]
fn batch_round_trip() {
    let mut c = canvas();
    let mut s = stack();
    c.add_layer("B").expect("add");
    c.add_layer("C").expect("add");
    let before = layer_ids(&c);

    // A group move: two single-layer reorders applied as one unit.
    c.reorder_layer(0, 2).expect("step 1");
    c.reorder_layer(0, 1).expect("step 2");
    let after = layer_ids(&c);
    assert_ne!(before, after);

    s.record(HistoryAction::Batch {
        label: "Reorder layers".to_string(),
        actions: vec![
            HistoryAction::LayerReorder { from: 0, to: 2 },
            HistoryAction::LayerReorder { from: 0, to: 1 },
        ],
    });

    s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo");
    assert_eq!(layer_ids(&c), before, "batch undo restores original order");

    s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo");
    assert_eq!(layer_ids(&c), after, "batch redo reapplies the moves");
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn batch_label_surfaces_through_undo() {
    let mut c = canvas();
    let mut s = stack();
    s.record(HistoryAction::Batch {
        label: "Delete group".to_string(),
        actions: vec![HistoryAction::LayerRename {
            id: layer_id(&c, 0),
            old_name: "Background".to_string(),
            new_name: "Background".to_string(),
        }],
    });
    let label = s.undo(&mut c, &mut ComponentLibrary::new()).expect("undo").expect("some label");
    assert_eq!(label, "Delete group");
    let label = s.redo(&mut c, &mut ComponentLibrary::new()).expect("redo").expect("some label");
    assert_eq!(label, "Delete group");
}

#[test]
#[ignore = "requires vulkan loader and device"]
fn component_actions_round_trip() {
    let mut c = canvas();
    let mut s = stack();
    let mut comps = ComponentLibrary::new();
    let a = comps.add_new("A");

    // Rename: forward sets new, backward restores old.
    comps.get_mut(&a).unwrap().name = "B".to_string();
    s.record(HistoryAction::ComponentRename {
        id: a.clone(),
        old_name: "A".to_string(),
        new_name: "B".to_string(),
    });
    s.undo(&mut c, &mut comps).expect("undo rename");
    assert_eq!(comps.get(&a).unwrap().name, "A");
    s.redo(&mut c, &mut comps).expect("redo rename");
    assert_eq!(comps.get(&a).unwrap().name, "B");

    // Add (as Duplicate does): component already present; undo removes, redo restores.
    let dup = comps.duplicate(&a).expect("duplicate");
    let idx = comps.components.iter().position(|x| x.id == dup).unwrap();
    let snapshot = comps.get(&dup).unwrap().to_snapshot();
    s.record(HistoryAction::ComponentAdd { index: idx, snapshot });
    assert_eq!(comps.len(), 2);
    s.undo(&mut c, &mut comps).expect("undo add");
    assert_eq!(comps.len(), 1);
    assert!(comps.get(&dup).is_none());
    s.redo(&mut c, &mut comps).expect("redo add");
    assert!(comps.get(&dup).is_some());

    // Remove: snapshot + remove, then record; undo restores at index, redo removes.
    let idx = comps.components.iter().position(|x| x.id == a).unwrap();
    let snapshot = comps.get(&a).unwrap().to_snapshot();
    comps.remove(&a);
    s.record(HistoryAction::ComponentRemove { index: idx, snapshot });
    assert!(comps.get(&a).is_none());
    s.undo(&mut c, &mut comps).expect("undo remove");
    assert!(comps.get(&a).is_some());
    assert_eq!(comps.components[idx].id, a, "restored at original index");
    s.redo(&mut c, &mut comps).expect("redo remove");
    assert!(comps.get(&a).is_none());
}
