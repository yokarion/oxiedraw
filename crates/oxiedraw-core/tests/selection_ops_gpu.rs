//! GPU integration tests for the selection mask's boolean operations, driven
//! through the public `Canvas` API. Each test is `#[ignore]` because it needs a
//! working Vulkan loader + device; run with
//! `cargo test -p oxiedraw-core --test selection_ops_gpu -- --ignored`.
//!
//! The four mask ops (Replace / Add / Subtract / Intersect) are separate
//! pipelines that differ only in their colour blend state, and they write the
//! R channel only because the mask is R8. A pipeline that widened that write
//! mask, or picked the wrong blend op, would corrupt the mask silently - the
//! composite would still render, just against the wrong region. These pin the
//! resulting region down by erasing through it and probing the alpha.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::selection::{RectShape, SelectionShape};
use oxiedraw_core::tools::SelectionMode;
use oxiedraw_utils::geometry::Size;

const SIZE: Size = Size {
    width: 16,
    height: 16,
};

/// Full-canvas opaque BGRA8 buffer.
fn opaque() -> Vec<u8> {
    let n = (SIZE.width * SIZE.height) as usize;
    let mut px = vec![0u8; n * 4];
    for chunk in px.chunks_exact_mut(4) {
        chunk.copy_from_slice(&[0, 0, 255, 255]);
    }
    px
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> SelectionShape {
    SelectionShape::Rect(RectShape { x, y, w, h })
}

/// Erase through the current selection and return an alpha probe at (x, y).
fn erase_and_probe(canvas: &mut Canvas, idx: usize) -> impl Fn(usize, usize) -> u8 {
    canvas.erase_selection_in_layer(idx).unwrap();
    let out = canvas.read_layer(idx).unwrap();
    move |x: usize, y: usize| out[(y * SIZE.width as usize + x) * 4 + 3]
}

/// Add: left half OR right half covers everything, so the whole layer erases.
#[test]
#[ignore = "requires vulkan loader and device"]
fn add_unions_two_regions() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &opaque()).unwrap();

    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 8.0, 16.0), SelectionMode::Replace)
        .unwrap();
    canvas
        .apply_selection_shape(&rect(8.0, 0.0, 8.0, 16.0), SelectionMode::Add)
        .unwrap();

    let alpha = erase_and_probe(&mut canvas, idx);
    assert_eq!(alpha(2, 8), 0, "left half should be erased");
    assert_eq!(alpha(13, 8), 0, "right half should be erased too (Add)");
}

/// Subtract: whole canvas minus the left half leaves only the right selected.
#[test]
#[ignore = "requires vulkan loader and device"]
fn subtract_removes_a_region() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &opaque()).unwrap();

    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 16.0, 16.0), SelectionMode::Replace)
        .unwrap();
    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 8.0, 16.0), SelectionMode::Subtract)
        .unwrap();

    let alpha = erase_and_probe(&mut canvas, idx);
    assert_eq!(alpha(2, 8), 255, "left half was subtracted, must survive");
    assert_eq!(alpha(13, 8), 0, "right half stays selected and erases");
}

/// Intersect: left half AND top half leaves only the top-left quadrant.
#[test]
#[ignore = "requires vulkan loader and device"]
fn intersect_keeps_only_the_overlap() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &opaque()).unwrap();

    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 8.0, 16.0), SelectionMode::Replace)
        .unwrap();
    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 16.0, 8.0), SelectionMode::Intersect)
        .unwrap();

    let alpha = erase_and_probe(&mut canvas, idx);
    assert_eq!(alpha(2, 2), 0, "top-left overlap should be erased");
    assert_eq!(alpha(2, 13), 255, "bottom-left is outside the intersection");
    assert_eq!(alpha(13, 2), 255, "top-right is outside the intersection");
    assert_eq!(alpha(13, 13), 255, "bottom-right is outside both");
}

/// The folder-icon path (`select_from_layers_alpha`) must select the union of
/// the layers' actual painted alpha, not their bounding box or the whole canvas.
#[test]
#[ignore = "requires vulkan loader and device"]
fn select_from_layers_alpha_follows_the_painted_shape() {
    let mut canvas = Canvas::headless(SIZE).unwrap();

    // Two sparse layers: one paints the top-left quadrant, one the bottom-right.
    let mut a = vec![0u8; (SIZE.width * SIZE.height) as usize * 4];
    let mut b = a.clone();
    for y in 0..8usize {
        for x in 0..8usize {
            let i = (y * SIZE.width as usize + x) * 4;
            a[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
    }
    for y in 8..16usize {
        for x in 8..16usize {
            let i = (y * SIZE.width as usize + x) * 4;
            b[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
    }
    let ia = canvas.add_layer_with_pixels("a", &a).unwrap();
    let ib = canvas.add_layer_with_pixels("b", &b).unwrap();

    // A third, fully opaque layer to erase through - it shows the mask shape.
    let target = canvas.add_layer_with_pixels("target", &opaque()).unwrap();
    canvas.select_from_layers_alpha(&[ia, ib]).unwrap();

    let alpha = erase_and_probe(&mut canvas, target);
    assert_eq!(alpha(2, 2), 0, "top-left is painted on layer a: selected");
    assert_eq!(alpha(13, 13), 0, "bottom-right is painted on layer b: selected");
    assert_eq!(
        alpha(13, 2),
        255,
        "top-right is unpainted on both layers - selecting it means the mask \
         took the bounding box or the whole canvas instead of the alpha shape"
    );
    assert_eq!(alpha(2, 13), 255, "bottom-left is unpainted on both layers");
}

/// A folder containing an adjustment layer must still select only the painted
/// artwork. An adjustment layer's slot is a grayscale mask that starts fully
/// opaque, so unioning its alpha in would select the entire canvas.
#[test]
#[ignore = "requires vulkan loader and device"]
fn adjustment_layer_in_the_set_does_not_select_everything() {
    let mut canvas = Canvas::headless(SIZE).unwrap();

    // Artwork covering only the top-left quadrant.
    let mut art = vec![0u8; (SIZE.width * SIZE.height) as usize * 4];
    for y in 0..8usize {
        for x in 0..8usize {
            let i = (y * SIZE.width as usize + x) * 4;
            art[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
    }
    let painted = canvas.add_layer_with_pixels("art", &art).unwrap();
    let adj = canvas.add_adjustment_layer("adj").unwrap();
    let target = canvas.add_layer_with_pixels("target", &opaque()).unwrap();

    // Exactly what the folder icon passes for a group holding both.
    canvas.select_from_layers_alpha(&[painted, adj]).unwrap();

    let alpha = erase_and_probe(&mut canvas, target);
    assert_eq!(alpha(2, 2), 0, "the painted quadrant should be selected");
    assert_eq!(
        alpha(13, 13),
        255,
        "unpainted area must stay unselected - the adjustment layer's full-canvas \
         mask leaked into the union"
    );
}

/// Replace really replaces rather than accumulating with what came before.
#[test]
#[ignore = "requires vulkan loader and device"]
fn replace_discards_the_previous_mask() {
    let mut canvas = Canvas::headless(SIZE).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &opaque()).unwrap();

    canvas
        .apply_selection_shape(&rect(0.0, 0.0, 8.0, 16.0), SelectionMode::Replace)
        .unwrap();
    canvas
        .apply_selection_shape(&rect(8.0, 0.0, 8.0, 16.0), SelectionMode::Replace)
        .unwrap();

    let alpha = erase_and_probe(&mut canvas, idx);
    assert_eq!(alpha(2, 8), 255, "the first (left) selection must be gone");
    assert_eq!(alpha(13, 8), 0, "only the second (right) selection erases");
}
