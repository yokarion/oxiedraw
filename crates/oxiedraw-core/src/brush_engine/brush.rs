use oxiedraw_utils::geometry::Point;

use crate::color::Color;

use super::{BrushFamily, InputSample};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BrushPresetId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeContext {
    pub preset: BrushPresetId,
    pub color: Color,
    pub size: f32,
    pub opacity: f32,
}

/// One brush stamp. Carries the full set of per-dab fields the renderer
/// can use. Fields a given `BrushFamily` doesn't read (e.g. rotation on
/// soft-round) are still present so the GPU instance layout stays
/// uniform across families.
#[derive(Debug, Clone, Copy)]
pub struct Dab {
    pub center: Point,
    pub radius: f32,
    /// Radians. Ignored by soft-round / pixel families.
    pub rotation: f32,
    /// Squish on the local Y axis. `1.0` = round.
    pub aspect: f32,
    /// Coverage multiplier, `0..=1`. Soft-round uses this to attenuate
    /// per-dab opacity without changing the stroke colour.
    pub flow: f32,
    /// Per-dab tint. Carries premultiplied colour for stage-3 hue/sat/val
    /// jitter; today every dab in a stroke shares the stroke colour.
    pub color: Color,
    /// `(u0, v0, u1, v1)` into the pattern atlas. Unused until stage 4.
    pub texture_uv: [f32; 4],
}

impl Dab {
    /// Plain round dab at the given centre/radius with the stroke colour.
    /// Use this when no dynamics or pattern fields are active.
    pub const fn round(center: Point, radius: f32, color: Color) -> Self {
        Self {
            center,
            radius,
            rotation: 0.0,
            aspect: 1.0,
            flow: 1.0,
            color,
            texture_uv: [0.0, 0.0, 1.0, 1.0],
        }
    }
}

pub trait PaintTarget {
    /// Set the GPU pipeline family for subsequent `paint_dabs` calls.
    /// Called by `BrushEngine` at the start of every input event so a
    /// fresh adapter knows which family to bind. Borrows the family so
    /// the `Rc` inside `Textured` is not cloned per push.
    fn set_family(&mut self, family: &BrushFamily);
    fn paint_dabs(&mut self, dabs: &[Dab]);
}

pub trait StrokeRenderer {
    fn push(&mut self, sample: InputSample, target: &mut dyn PaintTarget);
    fn end(&mut self, target: &mut dyn PaintTarget);
    fn preview(&self, _target: &mut dyn PaintTarget) {}
}
