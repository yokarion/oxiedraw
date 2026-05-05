//! GPU integration tests for deleting the pixels inside a selection, driven
//! through the public `Canvas` API. Each test is `#[ignore]` because it needs a
//! working Vulkan loader + device; run with `cargo test -p oxiedraw-core --test
//! selection_delete_gpu -- --ignored`.

#![allow(clippy::unwrap_used)]

use oxiedraw_core::canvas::Canvas;
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

fn select_left_half(canvas: &mut Canvas, size: Size) {
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
}

// Bug 1: Delete erases only the selected pixels and the selection must stay
// active afterwards (the marquee should not disappear).
#[test]
#[ignore = "requires vulkan loader and device"]
fn erase_selection_clears_pixels_but_keeps_selection() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 0, 0, 255)).unwrap();

    select_left_half(&mut canvas, size);
    assert!(canvas.selection_active(), "precondition: selection active");

    canvas.erase_selection_in_layer(idx).unwrap();

    // The selection must remain after the erase.
    assert!(canvas.selection_active(), "selection should survive a delete");

    let out = canvas.read_layer(idx).unwrap();
    let alpha = |x: usize, y: usize| out[(y * 16 + x) * 4 + 3];
    let red = |x: usize, y: usize| out[(y * 16 + x) * 4 + 2];

    // Inside the selection (left half) the pixels are erased (transparent).
    assert_eq!(alpha(2, 8), 0, "left side should be erased");
    // Outside the selection (right half) the pixels are untouched.
    assert_eq!(alpha(12, 8), 255, "right side should be intact");
    assert_eq!(red(12, 8), 255, "right side keeps its color");
}

// Contrast: the cut path's clear_selection_from_layer drops the selection,
// whereas erase_selection_in_layer keeps it. Locks in the difference.
#[test]
#[ignore = "requires vulkan loader and device"]
fn clear_selection_from_layer_deselects() {
    let size = Size::new(16, 16);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer_with_pixels("t", &solid(size, 0, 0, 255)).unwrap();

    select_left_half(&mut canvas, size);
    canvas.clear_selection_from_layer(idx).unwrap();

    assert!(!canvas.selection_active(), "cut-style clear should deselect");
}
