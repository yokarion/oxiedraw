//! Non-destructive adjustment-layer effects.
//!
//! An adjustment layer carries an ordered list of [`Effect`]s that are applied
//! to the composited result of everything below it (the accumulator), gated by
//! the layer's own mask. Effects are parameters only - no pixels - so they are
//! cheap to clone for history and serialize directly into the project file.
//! The actual pixel work runs on Vulkan; see `renderer/vulkan/adjust_ops.rs`.

use serde::{Deserialize, Serialize};

use crate::color::Color;

use std::sync::atomic::{AtomicU64, Ordering};

static EFFECT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Stable opaque id for an effect, used by history (to target a specific
/// effect across reorders) and the editor UI.
#[must_use]
pub fn generate_effect_id() -> String {
    let n = EFFECT_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("e{n:015x}")
}

/// Advance the effect-id counter past an id loaded from a project file. The
/// counter is process-global and resets to 1 each launch, so without this a
/// reopened document that adds an adjustment layer would re-mint ids that
/// collide with effects already in the file.
pub fn observe_effect_id(id: &str) {
    if let Some(hex) = id.strip_prefix('e')
        && let Ok(n) = u64::from_str_radix(hex, 16)
    {
        crate::document::bump_counter_past(&EFFECT_ID_COUNTER, n);
    }
}

/// How a stroke's edge is resolved against the alpha-distance field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StrokeSoftness {
    /// Hard threshold - the stroke renders pixelated, as-is.
    #[default]
    Pixelated,
    /// One-pixel linear ramp for an anti-aliased edge.
    Bilinear,
}

impl StrokeSoftness {
    pub const ALL: [Self; 2] = [Self::Pixelated, Self::Bilinear];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pixelated => "Pixelated",
            Self::Bilinear => "Bilinear",
        }
    }

    #[must_use]
    pub fn from_index(index: u32) -> Self {
        Self::ALL.get(index as usize).copied().unwrap_or_default()
    }

    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn to_index(self) -> u32 {
        Self::ALL.iter().position(|&s| s == self).unwrap_or(0) as u32
    }
}

/// An effect kind with its parameters. Each variant maps to one GPU pass
/// chain run against the accumulator.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum EffectKind {
    /// Hue rotation (degrees), saturation multiplier, brightness multiplier.
    /// Identity is `{ 0.0, 1.0, 1.0 }`. Maps to the existing `filter_hsv`
    /// shader (whose third param is the additive-brightness control).
    HueSatBright {
        hue_degrees: f32,
        saturation: f32,
        brightness: f32,
    },
    /// Box blur with independent horizontal / vertical radii in pixels - the
    /// same parameters as the destructive Blur filter. Maps to the separable
    /// `filter_box_blur`. The legacy single-`radius` field deserializes into
    /// `radius_x` (with `radius_y` defaulting to 0) so old projects still load.
    Blur {
        #[serde(alias = "radius")]
        radius_x: f32,
        #[serde(default)]
        radius_y: f32,
    },
    /// Invert colors. No parameters. Maps to the destructive Invert filter.
    Invert,
    /// Unsharp-mask sharpen. `amount` of 0 leaves the backdrop unchanged.
    /// Maps to the destructive Sharpen filter.
    Sharpen { amount: f32 },
    /// Outline traced around the alpha edge of the backdrop, gated by the
    /// adjustment mask. `offset` slides the band from fully inside (-1.0)
    /// through centred (0.0) to fully outside (+1.0) the edge.
    Stroke {
        color: Color,
        opacity: f32,
        thickness: f32,
        offset: f32,
        softness: StrokeSoftness,
    },
}

impl EffectKind {
    /// Human-readable name for the editor sidebar, history labels, and toasts.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::HueSatBright { .. } => "Hue/Saturation/Brightness",
            Self::Blur { .. } => "Blur",
            Self::Invert => "Invert",
            Self::Sharpen { .. } => "Sharpen",
            Self::Stroke { .. } => "Stroke",
        }
    }

    #[must_use]
    pub const fn hue_sat_bright_identity() -> Self {
        Self::HueSatBright {
            hue_degrees: 0.0,
            saturation: 1.0,
            brightness: 1.0,
        }
    }

    #[must_use]
    pub const fn blur_default() -> Self {
        Self::Blur {
            radius_x: 4.0,
            radius_y: 4.0,
        }
    }

    #[must_use]
    pub const fn sharpen_default() -> Self {
        Self::Sharpen { amount: 3.0 }
    }

    #[must_use]
    pub fn stroke_default() -> Self {
        Self::Stroke {
            color: Color { r: 0, g: 0, b: 0 },
            opacity: 1.0,
            thickness: 6.0,
            offset: 0.0,
            softness: StrokeSoftness::Bilinear,
        }
    }
}

/// One non-destructive effect in an adjustment layer's stack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    /// Stable id - survives reorders, keys history edits.
    pub id: String,
    /// Disabled effects stay in the stack but are skipped at composite time.
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub kind: EffectKind,
}

impl Effect {
    #[must_use]
    pub fn new(kind: EffectKind) -> Self {
        Self {
            id: generate_effect_id(),
            enabled: true,
            kind,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// The payload of a `LayerKind::Adjustment`: an ordered effect stack applied
/// bottom (index 0) to top. The mask lives in the layer's own image slot, not
/// here.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AdjustmentData {
    #[serde(default)]
    pub effects: Vec<Effect>,
}

impl AdjustmentData {
    /// `true` when there is nothing to apply (no effects, or all disabled).
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.effects.iter().all(|e| !e.enabled)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn effect_ids_are_unique() {
        let a = generate_effect_id();
        let b = generate_effect_id();
        assert_ne!(a, b);
    }

    // Seeding past a loaded id means the next minted id can't collide with it.
    #[test]
    fn observe_effect_id_seeds_past_loaded_id() {
        let value = |s: &str| u64::from_str_radix(s.strip_prefix('e').unwrap(), 16).unwrap();
        let loaded = "e0000000000ffff";
        observe_effect_id(loaded);
        let next = generate_effect_id();
        assert!(value(&next) > value(loaded), "minted {next} should exceed {loaded}");
    }

    // The whole stack must survive a JSON round-trip - it rides document.json.
    #[test]
    fn adjustment_data_round_trips_through_json() {
        let data = AdjustmentData {
            effects: vec![
                Effect::new(EffectKind::hue_sat_bright_identity()),
                Effect {
                    id: "e000000000000001".into(),
                    enabled: false,
                    kind: EffectKind::Blur {
                        radius_x: 12.0,
                        radius_y: 8.0,
                    },
                },
                Effect::new(EffectKind::Invert),
                Effect::new(EffectKind::sharpen_default()),
                Effect::new(EffectKind::stroke_default()),
            ],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AdjustmentData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, back);
    }

    // A missing `enabled` field defaults to true (forward-compat for files
    // written before the flag, and hand-edited stacks).
    #[test]
    fn effect_enabled_defaults_true() {
        let json = r#"{"id":"e1","kind":{"Blur":{"radius":3.0}}}"#;
        let effect: Effect = serde_json::from_str(json).unwrap();
        assert!(effect.enabled);
        // Legacy single `radius` maps onto the horizontal axis.
        assert_eq!(
            effect.kind,
            EffectKind::Blur {
                radius_x: 3.0,
                radius_y: 0.0
            }
        );
    }

    #[test]
    fn softness_index_round_trips() {
        for s in StrokeSoftness::ALL {
            assert_eq!(StrokeSoftness::from_index(s.to_index()), s);
        }
    }

    #[test]
    fn is_noop_when_all_disabled() {
        let mut data = AdjustmentData {
            effects: vec![Effect::new(EffectKind::blur_default())],
        };
        assert!(!data.is_noop());
        data.effects[0].enabled = false;
        assert!(data.is_noop());
    }
}
