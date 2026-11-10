use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::super::dynamics::{Curve, Dynamics};
use super::super::preset::{
    DEFAULT_AIRBRUSH_RATE, PaintMode, SmudgeMode, TextureAdjust, TexturingMode, TipShape,
};

/// Bumped to 6 for the smudge mode. New fields are `#[serde(default)]`,
/// so the loader still accepts older archives - see `load::load`.
pub const SCHEMA_VERSION: u32 = 6;
pub(super) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Manifest discriminator distinguishing brushes from other future TAR
/// archive types so the loader can reject the wrong file early.
pub(super) const KIND: &str = "brush";

/// Monotonic revision of the built-in brush *definitions*. Bump this
/// whenever a builtin factory changes in a way that should reach existing
/// installs (tuned spacing, new grain, etc.) without a schema change.
/// `seed_missing` re-writes any builtin whose on-disk revision is older.
pub const BUILTIN_REVISION: u32 = 16;

/// `manifest.json` - fast metadata read for the brush picker so we can
/// list brushes without fully deserialising `brush.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrushManifest {
    pub schema_version: u32,
    pub app_version: String,
    pub kind: String,
    pub name: String,
    /// Built-in definition revision this archive was written from. `0`
    /// for user brushes and pre-revision builtins (via serde default).
    #[serde(default)]
    pub builtin_revision: u32,
}

/// Family discriminator in `brush.json`. `Textured` / `ImageTip` carry the
/// filename(s) inside `patterns/` so loaders can fetch the PNG(s).
#[derive(Debug, Serialize, Deserialize)]
pub enum FamilyDoc {
    SoftRound,
    Pixel,
    Textured {
        pattern: String,
    },
    /// Stamped image tip plus an optional canvas-anchored grain texture.
    ImageTip {
        tip: String,
        #[serde(default)]
        grain: Option<String>,
    },
    /// Color-smudge brush - no pattern; the round tip is shaped by `hardness`.
    Smudge,
    /// Color-smudge brush with an image tip, grain texture or smear alpha.
    #[serde(alias = "SmudgeTextured")]
    SmudgeOptions {
        #[serde(default)]
        tip: Option<String>,
        #[serde(default)]
        grain: Option<String>,
        #[serde(default)]
        smear_alpha: bool,
        #[serde(default)]
        mode: SmudgeMode,
    },
}

/// `brush.json` - full data needed to reconstruct a `BrushPreset`.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrushDocument {
    pub family: FamilyDoc,
    pub default_size: f32,
    pub default_opacity: f32,
    pub spacing_ratio: f32,
    pub stabilizer: f32,
    #[serde(default)]
    pub speed_smoothing: f32,
    /// Pre-schema-4 build-up flag; still written so older builds keep it.
    #[serde(default)]
    pub buildup: bool,
    /// Wins over `buildup` when present.
    #[serde(default)]
    pub paint_mode: Option<PaintMode>,
    #[serde(default)]
    pub softness_curve: Option<Curve>,
    #[serde(default)]
    pub airbrush: bool,
    #[serde(default = "default_airbrush_rate")]
    pub airbrush_rate: f32,
    /// Edge falloff. Defaults to `1.0` (crisp) so pre-schema-2 archives
    /// keep their original hard edge.
    #[serde(default = "default_hardness")]
    pub hardness: f32,
    #[serde(default)]
    pub tip: TipShape,
    #[serde(default)]
    pub texture_scale: f32,
    #[serde(default)]
    pub texture_strength: f32,
    /// Grain composite mode. Defaults to `Multiply` for pre-schema-3 archives.
    #[serde(default)]
    pub texturing_mode: TexturingMode,
    #[serde(default)]
    pub texture_adjust: TextureAdjust,
    pub dynamics: Dynamics,
}

fn default_hardness() -> f32 {
    1.0
}

const fn default_airbrush_rate() -> f32 {
    DEFAULT_AIRBRUSH_RATE
}

/// Decoded archive contents. Patterns are RGBA8 (premultiplied) with
/// dimensions; icon stays as raw PNG bytes since the UI re-encodes it
/// for GTK textures anyway.
pub struct BrushPackage {
    pub manifest: BrushManifest,
    pub document: BrushDocument,
    /// Filename -> (`rgba_premul`, width, height).
    pub patterns: HashMap<String, (Vec<u8>, u32, u32)>,
    pub icon: Option<Vec<u8>>,
    /// Raw PNG bytes of the cached stroke preview, if the archive
    /// includes one. The display path (picker rows, editor large
    /// preview) treats the alpha channel as a mask and recolors with
    /// the theme foreground.
    pub preview: Option<Vec<u8>>,
}
