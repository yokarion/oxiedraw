use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::super::dynamics::Dynamics;

pub const SCHEMA_VERSION: u32 = 1;
pub(super) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Manifest discriminator distinguishing brushes from other future TAR
/// archive types so the loader can reject the wrong file early.
pub(super) const KIND: &str = "brush";

/// `manifest.json` - fast metadata read for the brush picker so we can
/// list brushes without fully deserialising `brush.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrushManifest {
    pub schema_version: u32,
    pub app_version: String,
    pub kind: String,
    pub name: String,
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
    pub dynamics: Dynamics,
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
