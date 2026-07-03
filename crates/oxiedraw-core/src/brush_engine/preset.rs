use std::path::PathBuf;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use super::BrushPresetId;
use super::dynamics::{Curve, DynSource, Dynamics, Mapping};
use super::pattern::PatternData;

/// Procedural footprint shape for the textured family. The grain texture
/// is masked by this tip so a stroke keeps a defined edge instead of a
/// hard-cut window of texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TipShape {
    #[default]
    Round,
    Square,
}

impl TipShape {
    /// Encoding passed to the GPU: `0.0` round, `1.0` square.
    pub const fn as_gpu(self) -> f32 {
        match self {
            Self::Round => 0.0,
            Self::Square => 1.0,
        }
    }
}

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
const ICON_COMICS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/icons/builtin-brush-icons/comics.png"
));
const ICON_REAL_BRUSH: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/icons/builtin-brush-icons/real_brush.png"
));

/// Side length of the synthesised grain tiles. Matches the pattern atlas
/// slice dimension so the upload resize is an identity - no resampling
/// artefacts on the crisp halftone dots.
const GRAIN_DIM: u32 = 512;

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
    /// Edge falloff, `0..=1`. `1.0` gives a crisp anti-aliased edge;
    /// lower values fade from nearer the centre for a soft/airbrush look.
    /// Applies to the soft-round tip and the procedural textured tip.
    pub hardness: f32,
    /// Footprint shape for the textured family. Ignored otherwise.
    pub tip: TipShape,
    /// Global grain tile size in canvas pixels. The textured shader
    /// samples the pattern at `canvas_position / texture_scale`, anchoring
    /// the grain in canvas space so it stays continuous across the whole
    /// stroke. `0.0` disables the grain. This is the "pattern scale"
    /// knob for the global-texture brushes.
    pub texture_scale: f32,
    /// How strongly the global grain gates coverage, `0..=1`.
    pub texture_strength: f32,
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
    /// Soft round brush - a gentle airbrush-like circle whose alpha
    /// falls off almost from the centre.
    pub fn default_round(id: BrushPresetId) -> Self {
        Self {
            id,
            name: "Default Round".into(),
            family: BrushFamily::SoftRound,
            default_size: 80.0,
            default_opacity: 1.0,
            // Tight spacing so the very soft dabs merge into an even stroke
            // instead of scalloping (a wider spacing shows the soft edge of
            // each dab as ripples along the line).
            spacing_ratio: 0.025,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            // Very soft: fade begins near the centre for the airbrush look.
            hardness: 0.02,
            tip: TipShape::Round,
            texture_scale: 0.0,
            texture_strength: 0.0,
            // Pressure drives both size AND opacity. Tying flow to pressure
            // stops low-pressure (small) dabs from punching full-opacity
            // specks through the faint falloff of the larger dabs via the
            // MAX blend - they fade in with pressure instead.
            dynamics: Dynamics {
                size: Some(Mapping::pressure_linear()),
                flow: Some(Mapping::pressure_linear()),
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
            // Crisp edge - the ink pen stays sharp.
            hardness: 1.0,
            tip: TipShape::Round,
            texture_scale: 0.0,
            texture_strength: 0.0,
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
            hardness: 1.0,
            tip: TipShape::Round,
            texture_scale: 0.0,
            texture_strength: 0.0,
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
            hardness: 1.0,
            tip: TipShape::Round,
            texture_scale: 0.0,
            texture_strength: 0.0,
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

    /// Chalk - a square-ish tip dragged over a global chalk-grit grain.
    /// The grain is anchored in canvas space, so the stroke shows one
    /// continuous, non-repeating chalky texture instead of stamped bumps.
    pub fn chalk(id: BrushPresetId) -> Self {
        let pattern = Rc::new(PatternData::chalk_grain(GRAIN_DIM));
        Self {
            id,
            name: "Chalk".into(),
            family: BrushFamily::Textured(pattern),
            default_size: 140.0,
            default_opacity: 1.0,
            spacing_ratio: 0.08,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            hardness: 0.72,
            tip: TipShape::Square,
            texture_scale: 200.0,
            texture_strength: 0.85,
            dynamics: Dynamics {
                size: Some(Mapping::pressure_linear()),
                ..Dynamics::default()
            },
            icon: Some(ICON_CHALK.to_vec()),
            preview: None,
            source_path: None,
        }
    }

    /// Comics - a soft round footprint filled with a global halftone dot
    /// grid, the screentone look. Dots are canvas-anchored so the grid is
    /// consistent across the whole stroke.
    pub fn comics(id: BrushPresetId) -> Self {
        let pattern = Rc::new(PatternData::halftone(GRAIN_DIM));
        Self {
            id,
            name: "Comics Halftone".into(),
            family: BrushFamily::Textured(pattern),
            default_size: 130.0,
            default_opacity: 1.0,
            spacing_ratio: 0.08,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            hardness: 0.6,
            tip: TipShape::Round,
            texture_scale: 160.0,
            texture_strength: 1.0,
            dynamics: Dynamics {
                size: Some(Mapping::pressure_linear()),
                ..Dynamics::default()
            },
            icon: Some(ICON_COMICS.to_vec()),
            preview: None,
            source_path: None,
        }
    }

    /// Real Brush - a nearly crisp-edged round brush whose deposit is
    /// driven by pressure and builds up where it passes over the same
    /// area, capped near 90%, like an ink/watercolour brush. A very subtle
    /// low-frequency wash adds life without graininess.
    pub fn real_brush(id: BrushPresetId) -> Self {
        let pattern = Rc::new(PatternData::soft_wash(GRAIN_DIM));
        Self {
            id,
            name: "Real Brush".into(),
            family: BrushFamily::Textured(pattern),
            default_size: 90.0,
            // Hard per-stroke opacity ceiling: build-up accumulates in the
            // stroke buffer and composites once at this value, so one stroke
            // can't exceed 90% however much it overlaps itself.
            default_opacity: 0.9,
            spacing_ratio: 0.04,
            stabilizer: 0.2,
            speed_smoothing: 0.0,
            // Build-up: passing over the same area accumulates opacity like
            // ink/watercolour.
            buildup: true,
            // Only ~1% soft: essentially a crisp edge.
            hardness: 0.99,
            tip: TipShape::Round,
            // Large, very subtle wash - a whisper of variation, not grain.
            texture_scale: 320.0,
            texture_strength: 0.15,
            // Pressure drives the per-dab deposit rate. Values are small
            // because dabs OVER-accumulate in the stroke buffer (~25 overlap
            // each point per pass), so a light touch stays faint and a firm
            // one fills toward the opacity cap; going over an area again
            // builds it up further, up to the 90% ceiling.
            dynamics: Dynamics {
                flow: Some(Mapping {
                    source: DynSource::Pressure,
                    curve: Curve::linear(),
                    range: (0.006, 0.15),
                    invert: false,
                }),
                ..Dynamics::default()
            },
            icon: Some(ICON_REAL_BRUSH.to_vec()),
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
            hardness: 1.0,
            tip: TipShape::Round,
            texture_scale: 0.0,
            texture_strength: 0.0,
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
