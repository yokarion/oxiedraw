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

/// How the canvas-space texture pattern modulates dab coverage, mirroring
/// Krita's texture option (`KisMaskingBrushCompositeOp`). Only the two
/// modes the built-in brushes use are implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TexturingMode {
    /// `coverage *= mix(1, pattern, strength)` - the classic grain darken.
    #[default]
    Multiply,
    /// `coverage = max(0, coverage - pattern * strength)` - carves holes,
    /// Krita's default and what Chalk_Soft uses.
    Subtract,
}

impl TexturingMode {
    /// Encoding passed to the GPU: `0.0` multiply, `1.0` subtract.
    pub const fn as_gpu(self) -> f32 {
        match self {
            Self::Multiply => 0.0,
            Self::Subtract => 1.0,
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
const ICON_CHARCOAL_PENCIL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/icons/builtin-brush-icons/charcoal_pencil.png"
));

// Predefined brush-tip + texture images extracted from Krita's default
// resource bundle (`Krita_4_Default_Resources.bundle`), converted to plain
// RGBA8. Chalk_Soft stamps the oil-bristle tip and subtracts the dotted
// paper texture. Compiled straight into the binary like the icons.
const TIP_OIL_BRISTLE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/builtin-brush-assets/oil_bristle.png"
));
const TEXTURE_DRAWED_DOTTED: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/builtin-brush-assets/10_drawed_dotted.png"
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
    /// Sample from a bitmap pattern used as a canvas-anchored grain behind a
    /// procedural tip. The `Rc` is shared with the pattern atlas; the
    /// renderer caches uploads by `Rc::as_ptr` so switching brushes
    /// mid-session doesn't re-upload the same data.
    Textured(Rc<PatternData>),
    /// Stamped image tip (mask sampled from `tip` in dab-local space),
    /// optionally modulated by a canvas-anchored `grain` texture. This is
    /// how Krita's predefined-tip brushes (e.g. Chalk_Soft) work. Shares the
    /// textured GPU pipeline; the renderer resolves both patterns to atlas
    /// slices.
    ImageTip {
        tip: Rc<PatternData>,
        grain: Option<Rc<PatternData>>,
    },
    /// Colour-smudge brush (Krita colorsmudge): each dab picks up the colour
    /// under it from the layer, blends it with the carried smudge colour, and
    /// deposits it (plus a little paint). Painted by a dedicated GPU path, not
    /// the mask pipelines, so it carries no pattern data - the tip is a round
    /// mask shaped by `hardness`.
    Smudge,
}

impl BrushFamily {
    /// Stable discriminator used to index renderer mask-pipeline arrays.
    /// `ImageTip` reuses the textured pipeline (slot 2); `Smudge` doesn't use
    /// the mask pipelines at all (it has its own GPU path) - map it to slot 0.
    pub const fn kind_index(&self) -> usize {
        match self {
            Self::SoftRound | Self::Smudge => 0,
            Self::Pixel => 1,
            Self::Textured(_) | Self::ImageTip { .. } => 2,
        }
    }

    /// True for the colour-smudge family, which is painted by the dedicated
    /// GPU smudge path rather than the stroke-buffer mask pipelines.
    pub const fn is_smudge(&self) -> bool {
        matches!(self, Self::Smudge)
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
    /// How the grain/texture pattern composites onto dab coverage.
    pub texturing_mode: TexturingMode,
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
    /// Default round brush, matching Krita's default paintbrush
    /// (`b) Basic-2 Opacity`): a circle mask with fade 0.5 (solid core, soft
    /// edge), tight 0.1 spacing, pressure driving both size and opacity.
    pub fn default_round(id: BrushPresetId) -> Self {
        Self {
            id,
            name: "Default Round".into(),
            family: BrushFamily::SoftRound,
            default_size: 80.0,
            default_opacity: 1.0,
            // Krita's default auto-brush spacing.
            spacing_ratio: 0.1,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            // Krita fade 0.5: a defined solid core with a soft falloff, not
            // the old near-centre airbrush fade.
            hardness: 0.5,
            tip: TipShape::Round,
            texture_scale: 0.0,
            texture_strength: 0.0,
            texturing_mode: TexturingMode::Multiply,
            // Pressure drives both size AND opacity (Krita PressureSize +
            // PressureOpacity, linear curves). Tying flow to pressure also
            // stops low-pressure (small) dabs from punching full-opacity
            // specks through the softer larger dabs via the MAX blend.
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
            texturing_mode: TexturingMode::Multiply,
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
            texturing_mode: TexturingMode::Multiply,
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
            texturing_mode: TexturingMode::Multiply,
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

    /// Chalk, matching Krita's `h) Chalk_Soft`: the oil-bristle image tip
    /// stamped along the stroke and the dotted paper texture subtracted from
    /// coverage (canvas-anchored), giving a broken dry-media edge and grain.
    /// Krita values: tip scale 0.45, spacing 0.06, texture SUBTRACT strength
    /// 1, brightness 0.55, contrast 0.6.
    pub fn chalk(id: BrushPresetId) -> Self {
        let tip = Rc::new(
            PatternData::tip_from_png_bytes(TIP_OIL_BRISTLE).expect("built-in tip decodes"),
        );
        let grain = Rc::new(
            PatternData::texture_from_png_bytes(TEXTURE_DRAWED_DOTTED, 0.55, 0.6, false)
                .expect("built-in texture decodes"),
        );
        Self {
            id,
            name: "Chalk".into(),
            family: BrushFamily::ImageTip {
                tip,
                grain: Some(grain),
            },
            default_size: 140.0,
            default_opacity: 1.0,
            spacing_ratio: 0.06,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            // Tip mask comes from the image, so procedural hardness/tip are
            // unused; keep neutral values.
            hardness: 1.0,
            tip: TipShape::Round,
            // Texture tile size in canvas px (Krita pattern scale 1 on the
            // 512px pattern).
            texture_scale: 512.0,
            texture_strength: 1.0,
            texturing_mode: TexturingMode::Subtract,
            dynamics: Dynamics {
                size: Some(Mapping::pressure_linear()),
                ..Dynamics::default()
            },
            icon: Some(ICON_CHALK.to_vec()),
            preview: None,
            source_path: None,
        }
    }

    /// Charcoal Pencil, matching Krita's `h) Charcoal_Pencil_Thin`: a soft
    /// (gaussian) round tip modulated by the `10_drawed_dotted` paper texture
    /// sampled in canvas space (MULTIPLY), giving the broken, grainy pencil
    /// line. Pressure drives size (Krita curve 0.498..1.0) and coverage (the
    /// opacity curve, mapped to per-dab flow), and light pressure scatters the
    /// dabs a little so a soft touch breaks up while a firm one stays solid.
    /// Krita values: tip diameter 6, texture scale 0.35, brightness 0,
    /// contrast 1, invert on, strength 1, scatter 0.09.
    pub fn charcoal_pencil(id: BrushPresetId) -> Self {
        let grain = Rc::new(
            PatternData::texture_from_png_bytes(TEXTURE_DRAWED_DOTTED, 0.0, 1.0, true)
                .expect("built-in texture decodes"),
        );
        Self {
            id,
            name: "Charcoal Pencil".into(),
            family: BrushFamily::Textured(grain),
            // Thin pencil - clearly the "thin" charcoal against the fatter
            // media brushes, while wide enough for the paper tooth to read.
            default_size: 24.0,
            default_opacity: 1.0,
            spacing_ratio: 0.1,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            // Fairly crisp tip so the paper tooth reads as distinct grains
            // (soft touch) and a dry, textured edge (firm) rather than a fuzzy
            // grey band. The texture supplies the break-up, not the tip.
            hardness: 0.85,
            tip: TipShape::Round,
            // Krita texture scale 0.35 on the 512px pattern -> canvas px tile.
            texture_scale: 180.0,
            texture_strength: 1.0,
            texturing_mode: TexturingMode::Multiply,
            dynamics: Dynamics {
                // Pressure -> size: the tip ramps from a near-zero hairline at
                // no pressure up to full radius at 80% pressure, then holds flat.
                // So light pressure gives a fine grainy tail and the tip opens
                // up smoothly as you press harder.
                size: Some(Mapping {
                    source: DynSource::Pressure,
                    curve: Curve::from_points(vec![
                        (0.0, 0.02),
                        (0.8, 1.0),
                        (1.0, 1.0),
                    ])
                    .expect("size curve valid"),
                    range: (0.0, 1.0),
                    invert: false,
                }),
                // Krita's opacity-vs-pressure curve, mapped to per-dab flow so
                // coverage ramps up with pressure (soft at a light touch,
                // solid when pressed).
                flow: Some(Mapping {
                    source: DynSource::Pressure,
                    curve: Curve::from_points(vec![
                        (0.0, 0.0),
                        (0.128_414, 0.100_402),
                        (0.473_896, 0.710_843),
                        (1.0, 1.0),
                    ])
                    .expect("opacity curve valid"),
                    range: (0.0, 1.0),
                    invert: false,
                }),
                // Scatter 0.09 x diameter at a light touch, fading to almost
                // nothing when pressed (Krita curve 1 -> 0.1135). Absolute px
                // tuned to the default diameter.
                scatter: Some(Mapping {
                    source: DynSource::Pressure,
                    curve: Curve::from_points(vec![(0.0, 1.0), (1.0, 0.113_537)])
                        .expect("scatter curve valid"),
                    range: (0.0, 2.2),
                    invert: false,
                }),
                // The charcoal character: texture strength falls from 1 at a
                // light touch to 0 when pressed (Krita's `Texture/Strength`
                // sensor curve 0,1 -> 1,0). So a light stroke is broken/grainy
                // and a firm stroke lays a solid, dark line - not the flat
                // grey grain the constant strength gave at every pressure.
                texture_strength: Some(Mapping {
                    source: DynSource::Pressure,
                    curve: Curve::from_points(vec![(0.0, 1.0), (1.0, 0.0)])
                        .expect("texture strength curve valid"),
                    range: (0.0, 1.0),
                    invert: false,
                }),
                ..Dynamics::default()
            },
            icon: Some(ICON_CHARCOAL_PENCIL.to_vec()),
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
            texturing_mode: TexturingMode::Multiply,
            dynamics: Dynamics {
                size: Some(Mapping::pressure_linear()),
                ..Dynamics::default()
            },
            icon: Some(ICON_COMICS.to_vec()),
            preview: None,
            source_path: None,
        }
    }

    /// Real Brush, a colour-smudge brush modelled on Krita's `i) Wet Paint`
    /// (colorsmudge engine, dulling mode): each dab picks up the colour under
    /// it, blends it into the carried smudge colour, and deposits that plus a
    /// little paint. Soft round tip (fade 0.5), tight spacing; pressure drives
    /// size, smudge rate, colour rate and pickup radius. Painted by the GPU
    /// smudge path, so it carries no pattern.
    pub fn real_brush(id: BrushPresetId) -> Self {
        let pressure = |lo: f32, hi: f32| Mapping {
            source: DynSource::Pressure,
            curve: Curve::linear(),
            range: (lo, hi),
            invert: false,
        };
        Self {
            id,
            name: "Real Brush".into(),
            family: BrushFamily::Smudge,
            default_size: 60.0,
            default_opacity: 1.0,
            // Krita Wet Paint spacing.
            spacing_ratio: 0.07,
            stabilizer: 0.2,
            speed_smoothing: 0.0,
            buildup: false,
            // Krita mask fade 0.5.
            hardness: 0.5,
            tip: TipShape::Round,
            texture_scale: 0.0,
            texture_strength: 0.0,
            texturing_mode: TexturingMode::Multiply,
            // Pressure drives size + the three smudge parameters (Krita has
            // PressureSize / PressureSmudgeRate / PressureColorRate /
            // PressureSmudgeRadius all on). Light touch smears faintly; firmer
            // pressure smears harder and lays down more paint.
            dynamics: Dynamics {
                size: Some(Mapping::pressure_linear()),
                // Colour rate stays small so the brush mostly smears existing
                // paint and lays down only a little of its own colour (a wet
                // brush, not a paintbrush). Smudge rate is left at its 1.0
                // default (constant) so the deposit strength does NOT pulse
                // with pressure - that pulsing beaded into dots on dark areas.
                color_rate: Some(pressure(0.015, 0.09)),
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
            texturing_mode: TexturingMode::Multiply,
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
