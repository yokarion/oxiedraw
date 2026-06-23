use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::{BlendMode, Layer, LayerKind};

/// Layer list + active selection. Index 0 is the bottom of the z-order stack.
///
/// Uses `Rc<RefCell<...>>` / `Rc<Cell<...>>` so UI callbacks can mutate
/// without taking `&mut Document`.
#[derive(Debug, Clone)]
pub struct LayerState {
    layers: Rc<RefCell<Vec<Layer>>>,
    active: Rc<Cell<Option<usize>>>,
}

impl LayerState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            layers: Rc::new(RefCell::new(Vec::new())),
            active: Rc::new(Cell::new(None)),
        }
    }

    /// Add a layer on top of the stack. Returns its new index.
    pub fn add(&self, name: impl Into<String>) -> usize {
        let mut layers = self.layers.borrow_mut();
        layers.push(Layer::new(name));
        layers.len() - 1
    }

    pub fn len(&self) -> usize {
        self.layers.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.borrow().is_empty()
    }

    pub fn snapshot(&self) -> Vec<Layer> {
        self.layers.borrow().clone()
    }

    pub fn active(&self) -> Option<usize> {
        self.active.get()
    }

    pub fn set_active(&self, index: Option<usize>) {
        self.active.set(index);
    }

    pub fn select_index(&self, index: usize) {
        self.active.set(Some(index));
    }

    /// Remove the layer at `index`. Active selection shifts to keep the same logical layer selected.
    pub fn remove(&self, index: usize) {
        let mut layers = self.layers.borrow_mut();
        if index >= layers.len() {
            return;
        }
        layers.remove(index);
        let new_len = layers.len();
        drop(layers);
        let new_active = self.active.get().and_then(|active| {
            if new_len == 0 {
                None
            } else if active == index {
                Some(index.min(new_len - 1))
            } else if active > index {
                Some(active - 1)
            } else {
                Some(active)
            }
        });
        self.active.set(new_active);
    }

    pub fn clear(&self) {
        self.layers.borrow_mut().clear();
        self.active.set(None);
    }

    pub(crate) fn add_full(&self, id: String, name: impl Into<String>, visible: bool) -> usize {
        let mut layers = self.layers.borrow_mut();
        layers.push(Layer::with_id(id, name, visible));
        layers.len() - 1
    }

    /// Replace the kind of the layer at `index`. No-op if out of range.
    pub fn set_kind(&self, index: usize, kind: LayerKind) {
        if let Some(layer) = self.layers.borrow_mut().get_mut(index) {
            layer.kind = kind;
        }
    }

    /// The kind of the layer at `index`, or `None` if out of range.
    pub fn kind(&self, index: usize) -> Option<LayerKind> {
        self.layers.borrow().get(index).map(|l| l.kind.clone())
    }

    /// Rename the layer at `index`. No-op if out of range.
    pub fn rename(&self, index: usize, new_name: impl Into<String>) {
        if let Some(layer) = self.layers.borrow_mut().get_mut(index) {
            layer.name = new_name.into();
        }
    }

    /// Toggle the `visible` flag on the layer at `index`. No-op if
    /// out of range.
    pub fn set_visible(&self, index: usize, visible: bool) {
        if let Some(layer) = self.layers.borrow_mut().get_mut(index) {
            layer.visible = visible;
        }
    }

    /// Set the blend mode + opacity of the layer at `index`. No-op if out of
    /// range. Opacity is clamped to `0.0..=1.0`.
    pub fn set_blend(&self, index: usize, blend: BlendMode, opacity: f32) {
        if let Some(layer) = self.layers.borrow_mut().get_mut(index) {
            layer.blend = blend;
            layer.opacity = opacity.clamp(0.0, 1.0);
        }
    }

    /// Blend mode + opacity of the layer at `index`, or `None` if out of range.
    pub fn blend(&self, index: usize) -> Option<(BlendMode, f32)> {
        self.layers.borrow().get(index).map(|l| (l.blend, l.opacity))
    }

    /// Move layer at `from` to position `to`. Adjusts the active
    /// index so the *same* layer stays selected after the move.
    /// No-op when either index is out of range.
    pub fn reorder(&self, from: usize, to: usize) {
        let mut layers = self.layers.borrow_mut();
        if from >= layers.len() || to >= layers.len() || from == to {
            return;
        }
        let item = layers.remove(from);
        layers.insert(to, item);
        drop(layers);

        // Track the active selection through the move so the user
        // doesn't see it jump after a reorder.
        if let Some(active) = self.active.get() {
            let new_active = if active == from {
                to
            } else if from < to && active > from && active <= to {
                active - 1
            } else if from > to && active >= to && active < from {
                active + 1
            } else {
                active
            };
            self.active.set(Some(new_active));
        }
    }
}

impl Default for LayerState {
    fn default() -> Self {
        Self::new()
    }
}
