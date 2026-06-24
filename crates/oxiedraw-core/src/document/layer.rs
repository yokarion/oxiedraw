use std::sync::atomic::{AtomicU64, Ordering};

use oxiedraw_utils::geometry::TransformRect;
use serde::{Deserialize, Serialize};

use crate::effects::AdjustmentData;
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

/// How a layer's pixels are composited over the layers below it. The integer
/// values are the contract with `layer_blend.frag` (see [`Self::to_gpu`]) and
/// must not be reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Addition,
    Darken,
    Screen,
    Overlay,
}

impl BlendMode {
    /// Every mode in dropdown order.
    pub const ALL: [Self; 6] = [
        Self::Normal,
        Self::Multiply,
        Self::Addition,
        Self::Darken,
        Self::Screen,
        Self::Overlay,
    ];

    /// Human-readable name for the UI dropdown.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Multiply => "Multiply",
            Self::Addition => "Addition",
            Self::Darken => "Darken",
            Self::Screen => "Screen",
            Self::Overlay => "Overlay",
        }
    }

    /// Index passed to the blend shader's push constant.
    #[must_use]
    pub const fn to_gpu(self) -> u32 {
        match self {
            Self::Normal => 0,
            Self::Multiply => 1,
            Self::Addition => 2,
            Self::Darken => 3,
            Self::Screen => 4,
            Self::Overlay => 5,
        }
    }

    /// Map a dropdown position back to a mode (clamped to `Normal`).
    #[must_use]
    pub fn from_index(index: u32) -> Self {
        Self::ALL.get(index as usize).copied().unwrap_or(Self::Normal)
    }

    /// Position of this mode in `ALL` (its dropdown row). Paired with
    /// `from_index`; kept separate from `to_gpu` so the UI order and the shader
    /// contract can change independently.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn to_index(self) -> u32 {
        Self::ALL.iter().position(|&m| m == self).unwrap_or(0) as u32
    }
}

/// What a layer holds.
///
/// `Raster` is the normal painted layer. `Component` is a pre-rendered,
/// rescalable instance of a component: its slot pixels are re-rendered from
/// the component's master texture. `Text` is an editable text box re-rendered
/// from its [`TextContent`]. `Adjustment` is a non-destructive effect layer:
/// it holds no color, its image slot is a grayscale mask, and at composite
/// time it filters everything below it. All non-raster kinds reject raster
/// operations except `Adjustment`, whose slot accepts brush strokes (to paint
/// the mask).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum LayerKind {
    #[default]
    Raster,
    Component(ComponentInstance),
    Text(TextContent),
    Adjustment(AdjustmentData),
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
            // The mask follows the layer's pixels (which crop already moves);
            // the effect parameters carry no canvas geometry.
            Self::Adjustment(data) => Self::Adjustment(data.clone()),
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
    /// How this layer composites over the layers below it.
    pub blend: BlendMode,
    /// Layer opacity in `0.0..=1.0` (1.0 = fully opaque).
    pub opacity: f32,
}

impl Layer {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: generate_layer_id(),
            name: name.into(),
            visible: true,
            kind: LayerKind::Raster,
            blend: BlendMode::Normal,
            opacity: 1.0,
        }
    }

    pub(crate) fn with_id(id: String, name: impl Into<String>, visible: bool) -> Self {
        Self {
            id,
            name: name.into(),
            visible,
            kind: LayerKind::Raster,
            blend: BlendMode::Normal,
            opacity: 1.0,
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

    /// `true` for adjustment (non-destructive effect) layers. Their image slot
    /// is a grayscale mask rather than color, so brush strokes are allowed but
    /// composited as a mask.
    #[must_use]
    pub const fn is_adjustment(&self) -> bool {
        matches!(self.kind, LayerKind::Adjustment(_))
    }

    /// The effect stack of this layer, if it is an adjustment layer.
    #[must_use]
    pub fn adjustment(&self) -> Option<&AdjustmentData> {
        match &self.kind {
            LayerKind::Adjustment(data) => Some(data),
            _ => None,
        }
    }

    /// Mutable access to the effect stack, if this is an adjustment layer.
    pub fn adjustment_mut(&mut self) -> Option<&mut AdjustmentData> {
        match &mut self.kind {
            LayerKind::Adjustment(data) => Some(data),
            _ => None,
        }
    }

    /// The component this instance renders, if any.
    #[must_use]
    pub fn component_id(&self) -> Option<&str> {
        match &self.kind {
            LayerKind::Component(inst) => Some(inst.component_id.as_str()),
            LayerKind::Raster | LayerKind::Text(_) | LayerKind::Adjustment(_) => None,
        }
    }

    /// The text content of this layer, if it is a text layer.
    #[must_use]
    pub fn text_content(&self) -> Option<&TextContent> {
        match &self.kind {
            LayerKind::Text(content) => Some(content),
            LayerKind::Raster | LayerKind::Component(_) | LayerKind::Adjustment(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BlendMode;

    // The dropdown index (to_index/from_index) round-trips for every mode and is
    // independent of the shader contract (to_gpu).
    #[test]
    fn blend_mode_dropdown_index_round_trips() {
        for mode in BlendMode::ALL {
            assert_eq!(BlendMode::from_index(mode.to_index()), mode);
        }
        // Normal is the clamp target for an out-of-range dropdown row.
        assert_eq!(BlendMode::from_index(99), BlendMode::Normal);
    }

    // to_gpu is the shader push value (0..5) and must stay fixed regardless of
    // dropdown ordering.
    #[test]
    fn blend_mode_gpu_indices_are_stable() {
        assert_eq!(BlendMode::Normal.to_gpu(), 0);
        assert_eq!(BlendMode::Multiply.to_gpu(), 1);
        assert_eq!(BlendMode::Addition.to_gpu(), 2);
        assert_eq!(BlendMode::Darken.to_gpu(), 3);
        assert_eq!(BlendMode::Screen.to_gpu(), 4);
        assert_eq!(BlendMode::Overlay.to_gpu(), 5);
    }
}
