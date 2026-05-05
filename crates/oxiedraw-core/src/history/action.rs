//! Atomic, reversible user actions. One variant per high-level mutation.
//!
//! Every variant carries enough state to apply (forward) AND invert (backward).
//! Pixel-mutating variants embed a [`LayerPatch`] with before+after slices of
//! the affected canvas region.
//!
//! Adding a new variant: the central `match` in
//! [`crate::history::stack::HistoryStack::apply_direction`] uses no wildcard
//! arm, so the compiler will refuse to build until every history site handles
//! the new variant.

use serde::{Deserialize, Serialize};

use super::snapshot::LayerPatch;
use crate::components::ComponentSnapshot;
use crate::document::{LayerKind, Placement};
use crate::text::TextContent;

/// Direction of replay. Forward = redo or first apply; Backward = undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

/// Selection mask snapshot - used for selection-change history entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionSnapshot {
    pub active: bool,
    /// R8 mask, `canvas_w` * `canvas_h` bytes. `None` if `active = false`.
    pub mask: Option<Vec<u8>>,
}

/// A single undoable action. Each variant is self-contained: enough state
/// for both forward and backward application.
///
/// `#[non_exhaustive]` so external crates can't accidentally match without
/// a wildcard. Internally we always match exhaustively - that's the
/// compiler check that keeps new tools in lock-step with the history layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
#[non_exhaustive]
pub enum HistoryAction {
    /// Brush stroke (or any commit_stroke-driven pixel write) on a single
    /// layer. Patch carries before+after BGRA8 of the changed region.
    Stroke {
        layer_id: String,
        patch: LayerPatch,
    },
    /// Bucket fill commit on a single layer.
    Fill {
        layer_id: String,
        patch: LayerPatch,
    },
    /// Shape-tool commit (rectangle / line / circle / triangle) on a single
    /// layer. Patch carries the before+after BGRA8 of the changed region.
    Shape {
        layer_id: String,
        patch: LayerPatch,
    },
    /// Clear-layer or selection-clear pixel write.
    Clear {
        layer_id: String,
        patch: LayerPatch,
    },
    /// Transform apply - pixels remapped into a single layer.
    Transform {
        layer_id: String,
        patch: LayerPatch,
    },
    /// Filter apply (HSV / invert / blur / sharpen) on a single layer. Patch
    /// carries the before+after BGRA8 of the changed region. Multi-layer
    /// filter applies are wrapped in a [`Self::Batch`] of these.
    Filter {
        layer_id: String,
        patch: LayerPatch,
    },
    /// New layer appended (or inserted at idx). Patch carries the layer's
    /// initial pixels; before is empty.
    LayerAdd {
        idx: usize,
        id: String,
        name: String,
        visible: bool,
        /// Raster, or a component instance (so placement survives undo/redo).
        layer_kind: LayerKind,
        /// Full-canvas BGRA8 pixels at time of add (e.g. paste content).
        pixels: Vec<u8>,
    },
    /// Layer removed. We keep enough state to recreate it on undo.
    LayerRemove {
        idx: usize,
        id: String,
        name: String,
        visible: bool,
        layer_kind: LayerKind,
        pixels: Vec<u8>,
    },
    /// Layer moved from `from` to `to` in the z-order.
    LayerReorder { from: usize, to: usize },
    /// Layer renamed (uses layer id to be stable across reorders).
    LayerRename {
        id: String,
        old_name: String,
        new_name: String,
    },
    /// Layer visibility toggled.
    LayerVisibility { id: String, old: bool, new: bool },
    /// A text layer was edited (typed, restyled, resized). The patch carries
    /// the slot pixel diff; the before/after content let undo/redo restore the
    /// layer's `Text` kind metadata in lock-step with the pixels.
    TextEdit {
        layer_id: String,
        patch: LayerPatch,
        before_content: Box<TextContent>,
        after_content: Box<TextContent>,
    },
    /// A component instance re-transformed (its placement changed). The patch
    /// carries the slot pixel diff; the placements let undo/redo restore the
    /// layer's `Component` kind metadata in lock-step with the pixels.
    ComponentRetransform {
        layer_id: String,
        component_id: String,
        patch: LayerPatch,
        before_placement: Placement,
        after_placement: Placement,
    },
    /// Layer duplicated. The new layer's pixels match the source's; we
    /// store the new id + index so undo can remove exactly that layer.
    LayerDuplicate {
        src_idx: usize,
        new_idx: usize,
        new_id: String,
        new_name: String,
        layer_kind: LayerKind,
        pixels: Vec<u8>,
    },
    /// Several layers merged into one. `folded` describes each removed
    /// layer (in original z-order) so undo can re-create them.
    LayerMerge {
        survivor_idx: usize,
        survivor_pre: Vec<u8>,
        survivor_post: Vec<u8>,
        folded: Vec<FoldedLayer>,
    },
    /// Selection mask state changed (marquee, select-all, invert, deselect,
    /// select-from-alpha, ...). Stored as full mask snapshots since masks
    /// are R8 (1/4 the size of BGRA8) and easy to capture.
    SelectionChange {
        before: SelectionSnapshot,
        after: SelectionSnapshot,
    },
    /// Canvas crop / resize. We store the *previous* full state of all
    /// layers (pixels + metadata) so undo can recreate it exactly,
    /// including off-canvas content that the crop discarded.
    CropCanvas {
        before_size: (u32, u32),
        after_size: (u32, u32),
        before_layers: Vec<CropLayer>,
        after_layers: Vec<CropLayer>,
        active_layer: Option<usize>,
    },
    /// A component in the document library was renamed.
    ComponentRename {
        id: String,
        old_name: String,
        new_name: String,
    },
    /// A component was added to the library (New or Duplicate). Forward inserts
    /// the snapshot at `index`; backward removes it by id.
    ComponentAdd {
        index: usize,
        snapshot: ComponentSnapshot,
    },
    /// A component was removed from the library. Forward removes it by id;
    /// backward re-inserts the snapshot at `index`.
    ComponentRemove {
        index: usize,
        snapshot: ComponentSnapshot,
    },
    /// Several actions applied as one undoable unit (e.g. deleting a group of
    /// layers). Forward applies `actions` in order; backward inverts them in
    /// reverse order so the canvas returns to its exact prior state.
    Batch {
        label: String,
        actions: Vec<Self>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldedLayer {
    pub idx: usize,
    pub id: String,
    pub name: String,
    pub visible: bool,
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CropLayer {
    pub id: String,
    pub name: String,
    pub visible: bool,
    pub pixels: Vec<u8>,
    /// Layer kind (Raster/Text/Component) with geometry in this snapshot's
    /// coordinate space, so undo/redo of a crop preserves non-raster layers.
    #[serde(default)]
    pub kind: LayerKind,
}

impl HistoryAction {
    /// Short label for UI display (menu hint, tooltip).
    pub fn label(&self) -> &str {
        match self {
            Self::Stroke { .. } => "Brush stroke",
            Self::Fill { .. } => "Bucket fill",
            Self::Shape { .. } => "Shape",
            Self::Clear { .. } => "Clear layer",
            Self::Transform { .. } => "Transform",
            Self::TextEdit { .. } => "Edit text",
            Self::ComponentRetransform { .. } => "Transform component",
            Self::Filter { .. } => "Filter",
            Self::LayerAdd { .. } => "Add layer",
            Self::LayerRemove { .. } => "Remove layer",
            Self::LayerReorder { .. } => "Reorder layer",
            Self::LayerRename { .. } => "Rename layer",
            Self::LayerVisibility { .. } => "Toggle layer visibility",
            Self::LayerDuplicate { .. } => "Duplicate layer",
            Self::LayerMerge { .. } => "Merge layers",
            Self::SelectionChange { .. } => "Selection",
            Self::CropCanvas { .. } => "Crop canvas",
            Self::ComponentRename { .. } => "Rename component",
            Self::ComponentAdd { .. } => "Add component",
            Self::ComponentRemove { .. } => "Remove component",
            Self::Batch { label, .. } => label,
        }
    }
}
