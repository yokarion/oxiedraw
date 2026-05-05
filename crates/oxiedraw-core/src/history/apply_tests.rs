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
    let (id, name, visible, _, _) = capture_layer(&mut c, new_idx).expect("capture");
    s.record(HistoryAction::LayerAdd { idx: new_idx, id: id.clone(), name, visible, layer_kind: crate::document::LayerKind::Raster, pixels: pixels.clone() });
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
    let (id, name, visible, _, _) = capture_layer(&mut c, extra).expect("capture");

    c.remove_layer(extra).expect("remove_layer");
    s.record(HistoryAction::LayerRemove { idx: extra, id: id.clone(), name, visible, layer_kind: crate::document::LayerKind::Raster, pixels: pixels.clone() });
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
    let (new_id, new_name, _, _, _) = capture_layer(&mut c, new_idx).expect("capture");
    let dup_pixels = c.read_layer(new_idx).expect("dup pixels");
    assert_eq!(dup_pixels, src_pixels, "duplicate copies pixels");
    s.record(HistoryAction::LayerDuplicate {
        src_idx: 0,
        new_idx,
        new_id: new_id.clone(),
        new_name,
        layer_kind: crate::document::LayerKind::Raster,
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
        pixels: top_pixels.clone(),
    }];

    c.merge_layers(&[0, 1]).expect("merge");
    let survivor_post = c.read_layer(0).expect("survivor post");
    assert_eq!(c.layers().len(), 1);
    s.record(HistoryAction::LayerMerge {
        survivor_idx: 0,
        survivor_pre,
        survivor_post: survivor_post.clone(),
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
            let (id, name, visible, kind, pixels) = capture_layer(&mut c, i)?;
            Some(CropLayer { id, name, visible, pixels, kind })
        })
        .collect();

    // Crop to the top-left 8x8 quadrant.
    c.apply_crop(CropRect::new(0.0, 0.0, 8.0, 8.0)).expect("crop");
    let after_size = (c.size().width, c.size().height);
    assert_eq!(after_size, (8, 8));
    let after_layers: Vec<CropLayer> = (0..c.layers().len())
        .filter_map(|i| {
            let (id, name, visible, kind, pixels) = capture_layer(&mut c, i)?;
            Some(CropLayer { id, name, visible, pixels, kind })
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
