//! Undo/redo history.
//!
//! See the module-level docs in `action` and `stack` for the
//! compiler-checked extensibility story: one big enum + one exhaustive
//! match site.
//!
//! Usage:
//! - Construct a single [`HistoryStack`] at app startup (lives in
//!   `EngineState`), seeded from [`HistoryConfig`] from user settings.
//! - On every successful user mutation, call `record(action)` with the
//!   appropriate [`HistoryAction`] variant.
//! - Wire `app.undo` / `app.redo` to call `undo(canvas)` / `redo(canvas)`.

mod action;
mod snapshot;
mod stack;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod apply_tests;

pub use action::{CropLayer, Direction, FoldedLayer, HistoryAction, SelectionSnapshot};
pub use snapshot::{LayerPatch, PatchBounds};
pub use stack::{DEFAULT_CAPACITY, HistoryConfig, HistoryEntry, HistoryStack};

use crate::canvas::Canvas;

/// Read a layer's identity + kind + full BGRA8 pixels for history capture.
///
/// Returns `(id, name, visible, kind, blend, opacity, pixels)`, or `None` if
/// `idx` is out of range or the GPU readback fails. Centralises the boilerplate
/// every layer-level history site (`LayerAdd`, `LayerRemove`,
/// `LayerDuplicate`, ...) needs.
#[must_use]
pub fn capture_layer(
    canvas: &mut Canvas,
    idx: usize,
) -> Option<(
    String,
    String,
    bool,
    crate::document::LayerKind,
    crate::document::BlendMode,
    f32,
    Vec<u8>,
)> {
    let (id, name, visible, kind, blend, opacity) = {
        let snap = canvas.layers().snapshot();
        let layer = snap.get(idx)?;
        (
            layer.id.clone(),
            layer.name.clone(),
            layer.visible,
            layer.kind.clone(),
            layer.blend,
            layer.opacity,
        )
    };
    let pixels = canvas.read_layer(idx).ok()?;
    Some((id, name, visible, kind, blend, opacity, pixels))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// Capacity enforced as a ring buffer - oldest entries drop when full.
    #[test]
    fn capacity_ring_buffer() {
        let mut stack = HistoryStack::new(HistoryConfig { capacity: 3 });
        for i in 0..5u32 {
            stack.record(HistoryAction::LayerReorder {
                from: i as usize,
                to: (i + 1) as usize,
            });
        }
        assert_eq!(stack.undo_len(), 3);
        assert!(stack.can_undo());
        assert!(!stack.can_redo());
    }

    /// Recording a new action clears the redo stack.
    #[test]
    fn record_clears_redo() {
        let mut stack = HistoryStack::new(HistoryConfig::default());
        stack.record(HistoryAction::LayerReorder { from: 0, to: 1 });
        // Simulate moving the entry to redo (undo would do this but needs a Canvas).
        let e = stack.entries().back().cloned().unwrap();
        let _ = stack;
        let mut stack = HistoryStack::from_parts(8, VecDeque::default(), vec![e]);
        assert!(stack.can_redo());
        stack.record(HistoryAction::LayerReorder { from: 2, to: 3 });
        assert!(!stack.can_redo());
    }

    /// LayerPatch::from_full_diff returns None when buffers are identical.
    #[test]
    fn patch_no_diff_is_none() {
        let buf = vec![0u8; 4 * 4 * 4];
        assert!(LayerPatch::from_full_diff(&buf, &buf, 4, 4).is_none());
    }

    /// LayerPatch::from_full_diff finds the tight AABB of differing pixels.
    #[test]
    fn patch_finds_tight_aabb() {
        let mut before = vec![0u8; 8 * 8 * 4];
        let mut after = before.clone();
        // Change a single pixel at (3, 5).
        let idx = (5 * 8 + 3) * 4;
        after[idx + 2] = 0xff;
        after[idx + 3] = 0xff;
        let patch = LayerPatch::from_full_diff(&before, &after, 8, 8).unwrap();
        assert_eq!(patch.bounds.x, 3);
        assert_eq!(patch.bounds.y, 5);
        assert_eq!(patch.bounds.w, 1);
        assert_eq!(patch.bounds.h, 1);
        assert_eq!(patch.before, vec![0, 0, 0, 0]);
        assert_eq!(patch.after, vec![0, 0, 0xff, 0xff]);
        let _ = (&mut before, &mut after); // silence warnings
    }
}
