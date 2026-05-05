//! Ring-buffer undo/redo stack.
//!
//! The central [`HistoryStack::apply_direction`] function pattern-matches on
//! every [`HistoryAction`] variant *without a wildcard arm* - that's the
//! compiler-checked guarantee that adding a new variant updates every code
//! path that must handle it.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::action::{Direction, HistoryAction};
use crate::canvas::Canvas;
use crate::components::ComponentLibrary;
use crate::document::{ComponentInstance, LayerKind};
use crate::renderer::RendererError;

/// Maximum number of undo entries kept in memory.
pub const DEFAULT_CAPACITY: usize = 256;

/// Tunables for the history stack. Persisted via app settings.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HistoryConfig {
    pub capacity: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CAPACITY,
        }
    }
}

/// One recorded entry in the undo/redo timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub action: HistoryAction,
}

#[derive(Debug)]
pub struct HistoryStack {
    undo: VecDeque<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    capacity: usize,
}

impl HistoryStack {
    pub fn new(config: HistoryConfig) -> Self {
        Self {
            undo: VecDeque::with_capacity(config.capacity.min(1024)),
            redo: Vec::new(),
            capacity: config.capacity.max(1),
        }
    }

    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Change capacity at runtime. Trims oldest entries if shrinking.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.undo.len() > self.capacity {
            self.undo.pop_front();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub const fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub const fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Push a new action onto the undo stack. Clears the redo stack
    /// (standard "doing something new invalidates the redo branch").
    pub fn record(&mut self, action: HistoryAction) {
        self.redo.clear();
        if self.undo.len() == self.capacity {
            self.undo.pop_front();
        }
        self.undo.push_back(HistoryEntry { action });
    }

    /// Pop the most recent undo entry, apply its inverse, and move it to the
    /// redo stack. Returns the action label on success, or `None` if empty.
    pub fn undo(
        &mut self,
        canvas: &mut Canvas,
        components: &mut ComponentLibrary,
    ) -> Result<Option<String>, RendererError> {
        let Some(entry) = self.undo.pop_back() else {
            return Ok(None);
        };
        let label = entry.action.label().to_string();
        apply_direction(canvas, components, &entry.action, Direction::Backward)?;
        self.redo.push(entry);
        Ok(Some(label))
    }

    /// Pop the most recent redo entry, re-apply it, and push back onto undo.
    /// Returns the action label on success, or `None` if empty.
    pub fn redo(
        &mut self,
        canvas: &mut Canvas,
        components: &mut ComponentLibrary,
    ) -> Result<Option<String>, RendererError> {
        let Some(entry) = self.redo.pop() else {
            return Ok(None);
        };
        let label = entry.action.label().to_string();
        apply_direction(canvas, components, &entry.action, Direction::Forward)?;
        if self.undo.len() == self.capacity {
            self.undo.pop_front();
        }
        self.undo.push_back(entry);
        Ok(Some(label))
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    // Test-only helpers for exercising the undo/redo wiring directly.
    #[cfg(test)]
    pub(super) const fn entries(&self) -> &VecDeque<HistoryEntry> {
        &self.undo
    }
    #[cfg(test)]
    pub(super) fn from_parts(
        capacity: usize,
        undo: VecDeque<HistoryEntry>,
        redo: Vec<HistoryEntry>,
    ) -> Self {
        Self {
            undo,
            redo,
            capacity: capacity.max(1),
        }
    }
}

impl Default for HistoryStack {
    fn default() -> Self {
        Self::new(HistoryConfig::default())
    }
}

/// Apply an action in the given direction. The central exhaustive match
/// over [`HistoryAction`] - adding a variant fails to compile here, which
/// is the compiler-checked safety net the user asked for.
fn apply_direction(
    canvas: &mut Canvas,
    components: &mut ComponentLibrary,
    action: &HistoryAction,
    direction: Direction,
) -> Result<(), RendererError> {
    match action {
        HistoryAction::Stroke { layer_id, patch }
        | HistoryAction::Fill { layer_id, patch }
        | HistoryAction::Shape { layer_id, patch }
        | HistoryAction::Clear { layer_id, patch }
        | HistoryAction::Transform { layer_id, patch }
        | HistoryAction::Filter { layer_id, patch } => {
            if let Some(idx) = find_layer_idx(canvas, layer_id) {
                patch.apply(canvas, idx, direction)?;
            }
            Ok(())
        }
        HistoryAction::TextEdit {
            layer_id,
            patch,
            before_content,
            after_content,
        } => {
            if let Some(idx) = find_layer_idx(canvas, layer_id) {
                patch.apply(canvas, idx, direction)?;
                let content = match direction {
                    Direction::Forward => after_content,
                    Direction::Backward => before_content,
                };
                canvas
                    .layers()
                    .set_kind(idx, LayerKind::Text((**content).clone()));
            }
            Ok(())
        }
        HistoryAction::ComponentRetransform {
            layer_id,
            component_id,
            patch,
            before_placement,
            after_placement,
        } => {
            if let Some(idx) = find_layer_idx(canvas, layer_id) {
                patch.apply(canvas, idx, direction)?;
                let placement = match direction {
                    Direction::Forward => *after_placement,
                    Direction::Backward => *before_placement,
                };
                canvas.layers().set_kind(
                    idx,
                    LayerKind::Component(ComponentInstance {
                        component_id: component_id.clone(),
                        placement,
                    }),
                );
            }
            Ok(())
        }
        HistoryAction::LayerAdd {
            idx,
            id,
            name,
            visible,
            layer_kind,
            pixels,
        } => match direction {
            Direction::Forward => {
                recreate_layer(canvas, *idx, id, name, *visible, layer_kind, pixels)?;
                Ok(())
            }
            Direction::Backward => {
                if let Some(cur) = find_layer_idx(canvas, id) {
                    canvas.remove_layer(cur)?;
                }
                Ok(())
            }
        },
        HistoryAction::LayerRemove {
            idx,
            id,
            name,
            visible,
            layer_kind,
            pixels,
        } => match direction {
            Direction::Forward => {
                if let Some(cur) = find_layer_idx(canvas, id) {
                    canvas.remove_layer(cur)?;
                }
                Ok(())
            }
            Direction::Backward => {
                recreate_layer(canvas, *idx, id, name, *visible, layer_kind, pixels)?;
                Ok(())
            }
        },
        HistoryAction::LayerReorder { from, to } => match direction {
            Direction::Forward => canvas.reorder_layer(*from, *to),
            Direction::Backward => canvas.reorder_layer(*to, *from),
        },
        HistoryAction::LayerRename {
            id,
            old_name,
            new_name,
        } => {
            if let Some(idx) = find_layer_idx(canvas, id) {
                let target = match direction {
                    Direction::Forward => new_name.as_str(),
                    Direction::Backward => old_name.as_str(),
                };
                canvas.layers().rename(idx, target);
            }
            Ok(())
        }
        HistoryAction::LayerVisibility { id, old, new } => {
            if let Some(idx) = find_layer_idx(canvas, id) {
                let v = match direction {
                    Direction::Forward => *new,
                    Direction::Backward => *old,
                };
                canvas.set_layer_visible(idx, v)?;
            }
            Ok(())
        }
        HistoryAction::LayerDuplicate {
            src_idx: _,
            new_idx,
            new_id,
            new_name,
            layer_kind,
            pixels,
        } => match direction {
            Direction::Forward => {
                recreate_layer(canvas, *new_idx, new_id, new_name, true, layer_kind, pixels)?;
                Ok(())
            }
            Direction::Backward => {
                if let Some(cur) = find_layer_idx(canvas, new_id) {
                    canvas.remove_layer(cur)?;
                }
                Ok(())
            }
        },
        HistoryAction::LayerMerge {
            survivor_idx,
            survivor_pre,
            survivor_post,
            folded,
        } => match direction {
            Direction::Forward => {
                canvas.restore_layer(*survivor_idx, survivor_post)?;
                for f in folded.iter().rev() {
                    if let Some(cur) = find_layer_idx(canvas, &f.id) {
                        canvas.remove_layer(cur)?;
                    }
                }
                Ok(())
            }
            Direction::Backward => {
                if let Some(idx) = find_layer_idx(canvas, "") {
                    let _ = idx; // unused
                }
                // Restore folded layers at their original indices. Merge bakes to
                // raster, so folded layers come back as plain raster layers.
                for f in folded {
                    recreate_layer(
                        canvas,
                        f.idx,
                        &f.id,
                        &f.name,
                        f.visible,
                        &LayerKind::Raster,
                        &f.pixels,
                    )?;
                }
                canvas.restore_layer(*survivor_idx, survivor_pre)?;
                Ok(())
            }
        },
        HistoryAction::SelectionChange { before, after } => {
            let target = match direction {
                Direction::Forward => after,
                Direction::Backward => before,
            };
            if target.active {
                if let Some(ref mask) = target.mask {
                    let shape = crate::selection::SelectionShape::Mask(mask.clone());
                    canvas.apply_selection_shape(&shape, crate::tools::SelectionMode::Replace)?;
                } else {
                    canvas.select_all()?;
                }
            } else {
                canvas.deselect();
            }
            Ok(())
        }
        HistoryAction::CropCanvas {
            before_size,
            after_size,
            before_layers,
            after_layers,
            active_layer,
        } => {
            let (target_size, target_layers) = match direction {
                Direction::Forward => (*after_size, after_layers),
                Direction::Backward => (*before_size, before_layers),
            };
            let cur_size = canvas.size();
            if cur_size.width != target_size.0 || cur_size.height != target_size.1 {
                // Going from cur size to target size: do an apply_crop with a
                // matching rect. The pixels are then overwritten below.
                use crate::tools::CropRect;
                #[allow(clippy::cast_precision_loss)]
                let rect = CropRect::new(
                    0.0,
                    0.0,
                    target_size.0 as f32,
                    target_size.1 as f32,
                );
                canvas.apply_crop(rect)?;
            }
            let layers: Vec<(String, String, bool, Vec<u8>)> = target_layers
                .iter()
                .map(|l| (l.id.clone(), l.name.clone(), l.visible, l.pixels.clone()))
                .collect();
            canvas.replace_all_layers(&layers)?;
            // replace_all_layers resets kinds to Raster; restore the snapshot's
            // kinds (geometry already in the target coordinate space).
            for (idx, l) in target_layers.iter().enumerate() {
                if !matches!(l.kind, crate::document::LayerKind::Raster) {
                    canvas.layers().set_kind(idx, l.kind.clone());
                }
            }
            if let Some(idx) = active_layer
                && *idx < canvas.layers().len()
            {
                canvas.layers().set_active(Some(*idx));
            }
            Ok(())
        }
        HistoryAction::ComponentRename { id, old_name, new_name } => {
            if let Some(c) = components.get_mut(id) {
                c.name = match direction {
                    Direction::Forward => new_name.clone(),
                    Direction::Backward => old_name.clone(),
                };
            }
            Ok(())
        }
        HistoryAction::ComponentAdd { index, snapshot } => {
            match direction {
                Direction::Forward => components.insert_snapshot(*index, snapshot),
                Direction::Backward => {
                    components.remove(&snapshot.id);
                }
            }
            Ok(())
        }
        HistoryAction::ComponentRemove { index, snapshot } => {
            match direction {
                Direction::Forward => {
                    components.remove(&snapshot.id);
                }
                Direction::Backward => components.insert_snapshot(*index, snapshot),
            }
            Ok(())
        }
        HistoryAction::Batch { actions, .. } => match direction {
            Direction::Forward => {
                for a in actions {
                    apply_direction(canvas, components, a, Direction::Forward)?;
                }
                Ok(())
            }
            Direction::Backward => {
                for a in actions.iter().rev() {
                    apply_direction(canvas, components, a, Direction::Backward)?;
                }
                Ok(())
            }
        },
    }
}

fn find_layer_idx(canvas: &Canvas, id: &str) -> Option<usize> {
    canvas.layers().snapshot().iter().position(|l| l.id == id)
}

/// Recreate a layer with the given id/name/visibility/pixels at index
/// `target_idx`. The layer is appended on top, then reordered into place.
fn recreate_layer(
    canvas: &mut Canvas,
    target_idx: usize,
    id: &str,
    name: &str,
    visible: bool,
    kind: &LayerKind,
    pixels: &[u8],
) -> Result<(), RendererError> {
    // Rebuild the whole stack with the layer reinserted at target_idx.
    // `replace_all_layers` resets every kind to Raster, so we capture the
    // existing kinds (and the recreated layer's) and re-apply them after.
    let snap = canvas.layers().snapshot();
    let mut entries: Vec<(String, String, bool, Vec<u8>)> = Vec::with_capacity(snap.len() + 1);
    let mut kinds: Vec<LayerKind> = Vec::with_capacity(snap.len() + 1);
    for (i, l) in snap.iter().enumerate() {
        if i == target_idx {
            entries.push((id.to_string(), name.to_string(), visible, pixels.to_vec()));
            kinds.push(kind.clone());
        }
        let px = canvas.read_layer(i)?;
        entries.push((l.id.clone(), l.name.clone(), l.visible, px));
        kinds.push(l.kind.clone());
    }
    if target_idx >= snap.len() {
        entries.push((id.to_string(), name.to_string(), visible, pixels.to_vec()));
        kinds.push(kind.clone());
    }
    canvas.replace_all_layers(&entries)?;
    for (i, k) in kinds.into_iter().enumerate() {
        canvas.layers().set_kind(i, k);
    }
    Ok(())
}
