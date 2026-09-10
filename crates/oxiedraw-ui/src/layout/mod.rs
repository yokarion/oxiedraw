//! The layout model: a split tree of panels around one canvas, the rules each
//! panel docks by, and how an arrangement is stored. Pure model, no GTK.

mod floating;
mod panel;
pub(crate) mod presets;
mod store;
mod tree;

pub(crate) use floating::{
    Corner, FloatAnchor, FloatSize, MAX_FLOAT_SIZE, MIN_FLOAT_SIZE, ToolWindowId,
};
pub(crate) use panel::{Axis, DockSide, PanelId, SplitChild};
pub(crate) use store::{Layout, LayoutSettings};
pub(crate) use tree::LayoutNode;
