use std::path::PathBuf;
use std::rc::Rc;

use super::BrushPresetId;
use super::dynamics::{Curve, DynSource, Dynamics, Mapping};
use super::pattern::PatternData;

// Built-in brush icons live in `data/icons/builtin-brush-icons/` at the
// repo root. `include_bytes!` compiles them straight into the binary so
// `seed_missing` can write them into each `.oxiebrush` archive on first
// launch - no asset loading at runtime.
const ICON_DEFAULT_ROUND: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/icons/builtin-brush-icons/default_brush.png"
));
const ICON_INK_PEN: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/icons/builtin-brush-icons/ink_pen.png"
));
const ICON_PIXEL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/icons/builtin-brush-icons/pixel.png"
));
const ICON_SCATTER_DOT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/icons/builtin-brush-icons/scatter_dot.png"
));
const ICON_SPEED_BRUSH: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/icons/builtin-brush-icons/speed_brush.png"
));
const ICON_CHALK: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/icons/builtin-brush-icons/chalk.png"
));

/// GPU pipeline family a preset stamps with.
#[derive(Debug, Clone)]
pub enum BrushFamily {
    /// Anti-aliased round dab, the historical default.
    SoftRound,
    /// Hard-edged dab snapped to integer pixel centres. Dynamics are
    /// disabled for this family by convention (the curves don't match
    /// the pixel grid).
    Pixel,
    /// Sample from a bitmap pattern. The `Rc` is shared with the
    /// pattern atlas; the renderer caches uploads by `Rc::as_ptr` so
    /// switching brushes mid-session doesn't re-upload the same data.
    Textured(Rc<PatternData>),
}

impl BrushFamily {
    /// Stable discriminator used to index renderer pipeline arrays.
    pub const fn kind_index(&self) -> usize {
        match self {
            Self::SoftRound => 0,
            Self::Pixel => 1,
            Self::Textured(_) => 2,
        }
    }

    /// Total number of pipeline slots the renderer reserves.
    pub const COUNT: usize = 3;
}

/// Data-driven brush definition. Replaces the per-brush `impl Brush`
/// boilerplate - presets are plain values, loadable from `.oxiebrush`
/// archives later.
#[derive(Debug, Clone)]
pub struct BrushPreset {
    pub id: BrushPresetId,
    pub name: String,
    pub family: BrushFamily,
    pub default_size: f32,
    pub default_opacity: f32,
    pub spacing_ratio: f32,
    pub stabilizer: f32,
    /// EMA smoothing for the speed signal. `0.0` = baseline anti-jitter
    /// only; `1.0` = very heavy smoothing for calligraphic effects.
    pub speed_smoothing: f32,
    /// Build-up (airbrush) mode. When true, every motion-event stamp is
    /// composited into the active layer immediately so dragging back
    /// over the same spot during a single stroke accumulates opacity
    /// instead of saturating at the brush's MAX-blended dab.
    pub buildup: bool,
    pub dynamics: Dynamics,
    /// Optional custom icon shown in the brush picker. Raw PNG bytes,
    /// decoded lazily by the UI. `None` -> picker uses a generic placeholder.
    pub icon: Option<Vec<u8>>,
    /// Cached stroke preview rendered by the actual engine on a
    /// headless canvas, stored as RGBA PNG bytes. The display path uses
    /// the alpha channel as a mask and recolours with the theme
    /// foreground colour, so the cached image is colour-neutral.
    /// `None` for brushes that haven't been rendered yet - UI falls
    /// back to a Cairo approximation until the cache is filled.
    pub preview: Option<Vec<u8>>,
    /// Where this brush was loaded from on disk. `None` for hardcoded
    /// factories and `BrushPreset`s constructed in code. The brush
    /// manager uses this to save edits back to the correct file and
    /// to delete the right archive.
    pub source_path: Option<PathBuf>,
}

impl BrushPreset {
    /// Soft round brush, identical to the old `DefaultBrush`.
    pub fn default_round(id: BrushPresetId) -> Self {
        Self {
            id,
            name: "Default Round".into(),
            family: BrushFamily::SoftRound,
            default_size: 80.0,
            default_opacity: 1.0,
            spacing_ratio: 0.1,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            // Linear pressure -> radius, matching the legacy behaviour.
            dynamics: Dynamics {
                size: Some(Mapping::pressure_linear()),
                ..Dynamics::default()
            },
            icon: Some(ICON_DEFAULT_ROUND.to_vec()),
            preview: None,
            source_path: None,
        }
    }

    /// Ink pen, identical to the old `InkPenBrush`.
    pub fn ink_pen(id: BrushPresetId) -> Self {
        Self {
            id,
            name: "Ink Pen".into(),
            family: BrushFamily::SoftRound,
            default_size: 80.0,
            default_opacity: 1.0,
            spacing_ratio: 0.05,
            stabilizer: 0.65,
            speed_smoothing: 0.0,
            buildup: false,
            dynamics: Dynamics {
                size: Some(Mapping::pressure_linear()),
                ..Dynamics::default()
            },
            icon: Some(ICON_INK_PEN.to_vec()),
            preview: None,
            source_path: None,
        }
    }

    /// Hard-edged 1-pixel brush for validating the Pixel family pipeline.
    /// Bigger sizes produce hard-edged circles snapped to the pixel grid.
    pub fn pixel(id: BrushPresetId) -> Self {
        Self {
            id,
            name: "Pixel".into(),
            family: BrushFamily::Pixel,
            default_size: 1.0,
            default_opacity: 1.0,
            spacing_ratio: 0.5,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            dynamics: Dynamics::default(),
            icon: Some(ICON_PIXEL.to_vec()),
            preview: None,
            source_path: None,
        }
    }

    /// Scatter demo - pressure still drives size, but each dab is
    /// offset randomly within +/-0.6 x base diameter from the path.
    /// Spacing widens so individual dots are visible.
    pub fn scatter_dot(id: BrushPresetId) -> Self {
        Self {
            id,
            name: "Scatter Dot".into(),
            family: BrushFamily::SoftRound,
            default_size: 140.0,
            default_opacity: 1.0,
            spacing_ratio: 0.6,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            dynamics: Dynamics {
                size: Some(Mapping::pressure_linear()),
                scatter: Some(Mapping {
                    source: DynSource::Random,
                    curve: Curve::linear(),
                    range: (0.0, 18.0),
                    invert: false,
                }),
                ..Dynamics::default()
            },
            icon: Some(ICON_SCATTER_DOT.to_vec()),
            preview: None,
            source_path: None,
        }
    }

    /// Textured demo - soft-edged "chalk" stamp synthesised in code
    /// (see `PatternData::debug_chalk`). Validates the pattern atlas
    /// upload + textured pipelines without needing an asset on disk.
    pub fn debug_chalk(id: BrushPresetId) -> Self {
        let pattern = Rc::new(PatternData::debug_chalk(128));
        Self {
            id,
            name: "Chalk".into(),
            family: BrushFamily::Textured(pattern),
            default_size: 140.0,
            default_opacity: 1.0,
            spacing_ratio: 0.1,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            dynamics: Dynamics {
                size: Some(Mapping::pressure_linear()),
                ..Dynamics::default()
            },
            icon: Some(ICON_CHALK.to_vec()),
            preview: None,
            source_path: None,
        }
    }

    /// Speed demo - diameter scales *inverse* to stroke speed, so
    /// slow strokes paint fat and fast strokes paint thin (the
    /// calligraphic "speed pen" feel).
    pub fn speed_brush(id: BrushPresetId) -> Self {
        Self {
            id,
            name: "Speed Brush".into(),
            family: BrushFamily::SoftRound,
            default_size: 100.0,
            default_opacity: 1.0,
            spacing_ratio: 0.0,
            stabilizer: 0.0,
            speed_smoothing: 0.5,
            buildup: false,
            dynamics: Dynamics {
                size: Some(Mapping {
                    source: DynSource::Speed,
                    curve: Curve::linear(),
                    // Output is the diameter multiplier. Slow (speed~=0)
                    // -> 1.0 x base; fast (speed~=1) -> 0.15 x base.
                    range: (1.0, 0.15),
                    invert: false,
                }),
                ..Dynamics::default()
            },
            icon: Some(ICON_SPEED_BRUSH.to_vec()),
            preview: None,
            source_path: None,
        }
    }
}
