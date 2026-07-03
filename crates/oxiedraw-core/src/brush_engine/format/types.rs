use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::super::dynamics::Dynamics;
use super::super::preset::TipShape;

/// Bumped to 2 when the global-texture brush fields (hardness, tip,
/// texture scale/strength) landed. New fields are `#[serde(default)]`, so
/// the loader still accepts older archives - see `load::load`.
pub const SCHEMA_VERSION: u32 = 2;
pub(super) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Manifest discriminator distinguishing brushes from other future TAR
/// archive types so the loader can reject the wrong file early.
pub(super) const KIND: &str = "brush";

/// Monotonic revision of the built-in brush *definitions*. Bump this
/// whenever a builtin factory changes in a way that should reach existing
/// installs (tuned spacing, new grain, etc.) without a schema change.
/// `seed_missing` re-writes any builtin whose on-disk revision is older.
pub const BUILTIN_REVISION: u32 = 6;

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

/// Family discriminator in `brush.json`. `Textured` carries the
/// filename inside `patterns/` so loaders can fetch the PNG.
#[derive(Debug, Serialize, Deserialize)]
pub enum FamilyDoc {
    SoftRound,
    Pixel,
    Textured { pattern: String },
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
    #[serde(default)]
    pub buildup: bool,
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
    pub dynamics: Dynamics,
}

fn default_hardness() -> f32 {
    1.0
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
    /// preview) treats the alpha channel as a mask and recolours with
    /// the theme foreground.
    pub preview: Option<Vec<u8>>,
}
