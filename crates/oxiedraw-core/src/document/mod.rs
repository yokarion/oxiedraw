mod grouping;
mod layer;
mod properties;
mod state;

pub use grouping::{
    build_composite_steps, collect_leaf_ids, CompositeStep, LayerGroup, LayerTreeNode,
};
pub use layer::{BlendMode, ComponentInstance, Layer, LayerKind, Placement};
pub(crate) use layer::{bump_counter_past, generate_layer_id, observe_layer_id};
pub use properties::DocumentProperties;
pub use state::LayerState;

use oxiedraw_utils::geometry::Size;

#[derive(Debug, Clone)]
pub struct Document {
    pub properties: DocumentProperties,
    pub layers: LayerState,
}

impl Document {
    pub fn new(canvas: Size) -> Self {
        let layers = LayerState::new();
        layers.add("Background");
        layers.set_active(Some(0));
        Self {
            properties: DocumentProperties { canvas, dpi: 96.0 },
            layers,
        }
    }
}
