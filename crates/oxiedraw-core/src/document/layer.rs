use std::sync::atomic::{AtomicU64, Ordering};

use oxiedraw_utils::geometry::TransformRect;
use serde::{Deserialize, Serialize};

use crate::text::TextContent;

static LAYER_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(crate) fn generate_layer_id() -> String {
    let n = LAYER_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{n:016x}")
}

/// Where a component instance sits on the canvas. Centre-based with a
/// rotation angle, mirroring [`TransformRect`] so the existing affine remap
/// can render the master texture into the instance's layer slot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub angle: f32,
}

impl Placement {
    #[must_use]
    pub const fn new(cx: f32, cy: f32, w: f32, h: f32, angle: f32) -> Self {
        Self {
            cx,
            cy,
            w,
            h,
            angle,
        }
    }

    #[must_use]
    pub const fn to_rect(self) -> TransformRect {
        TransformRect::new(self.cx, self.cy, self.w, self.h, self.angle)
    }

    #[must_use]
    pub const fn from_rect(r: TransformRect) -> Self {
        Self {
            cx: r.cx,
            cy: r.cy,
            w: r.w,
            h: r.h,
            angle: r.angle,
        }
    }
}

/// Link from an instance layer back to the component it renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentInstance {
    /// Matches [`crate::components::Component::id`].
    pub component_id: String,
    pub placement: Placement,
}

/// What a layer holds.
///
/// `Raster` is the normal painted layer. `Component` is a pre-rendered,
/// rescalable instance of a component: its slot pixels are re-rendered from
/// the component's master texture. `Text` is an editable text box re-rendered
/// from its [`TextContent`]. Both non-raster kinds reject raster operations.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum LayerKind {
    #[default]
    Raster,
    Component(ComponentInstance),
    Text(TextContent),
}

impl LayerKind {
    /// Return a copy with any positional geometry shifted by `(dx, dy)` canvas
    /// pixels. Crop moves every layer's pixels by the crop offset, so text
    /// boxes and component placements must move with them to stay aligned.
    #[must_use]
    pub fn translated(&self, dx: f32, dy: f32) -> Self {
        match self {
            Self::Raster => Self::Raster,
            Self::Component(inst) => {
                let mut inst = inst.clone();
                inst.placement.cx += dx;
                inst.placement.cy += dy;
                Self::Component(inst)
            }
            Self::Text(content) => {
                let mut content = content.clone();
                content.box_rect.cx += dx;
                content.box_rect.cy += dy;
                Self::Text(content)
            }
        }
    }
}

/// A document layer. z-order is positional (index 0 = bottom of stack).
/// `id` is stable across save/load and names the layer's PNG in the archive.
#[derive(Debug, Clone)]
pub struct Layer {
    pub id: String,
    pub name: String,
    pub visible: bool,
    pub kind: LayerKind,
}

impl Layer {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: generate_layer_id(),
            name: name.into(),
            visible: true,
            kind: LayerKind::Raster,
        }
    }

    pub(crate) fn with_id(id: String, name: impl Into<String>, visible: bool) -> Self {
        Self {
            id,
            name: name.into(),
            visible,
            kind: LayerKind::Raster,
        }
    }

    /// `true` for component instance layers (raster ops are rejected on these).
    #[must_use]
    pub const fn is_component(&self) -> bool {
        matches!(self.kind, LayerKind::Component(_))
    }

    /// `true` for text layers (raster ops are rejected on these).
    #[must_use]
    pub const fn is_text(&self) -> bool {
        matches!(self.kind, LayerKind::Text(_))
    }

    /// `true` for any non-raster layer (raster ops are rejected on these).
    #[must_use]
    pub const fn is_raster(&self) -> bool {
        matches!(self.kind, LayerKind::Raster)
    }

    /// The component this instance renders, if any.
    #[must_use]
    pub fn component_id(&self) -> Option<&str> {
        match &self.kind {
            LayerKind::Component(inst) => Some(inst.component_id.as_str()),
            LayerKind::Raster | LayerKind::Text(_) => None,
        }
    }

    /// The text content of this layer, if it is a text layer.
    #[must_use]
    pub fn text_content(&self) -> Option<&TextContent> {
        match &self.kind {
            LayerKind::Text(content) => Some(content),
            LayerKind::Raster | LayerKind::Component(_) => None,
        }
    }
}
