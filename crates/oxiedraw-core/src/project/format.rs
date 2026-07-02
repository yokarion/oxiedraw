use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::document::{BlendMode, LayerKind, LayerTreeNode};
use crate::text::fonts::FontMeta;

/// Current writer schema. Loaders accept any version listed in
/// [`SUPPORTED_SCHEMA_VERSIONS`] and migrate as needed.
///
/// v3 adds the per-document component library (`components.json` +
/// `components/<id>/layers/<id>.png`) and a `kind` on each main layer entry.
/// v4 adds text layers (`LayerKind::Text`, stored inline in `kind`) and the
/// embedded font files they use (`fonts.json` + `fonts/<hash>`).
/// v5 adds per-layer `blend` mode and `opacity`.
/// v6 adds adjustment layers (`LayerKind::Adjustment`, effect stack stored
/// inline in `kind`); the layer's grayscale mask rides the existing
/// `layers/<id>.png`, so no new archive entries are needed.
/// v7 adds the layer folder tree (`layer_tree`), so adjustment layers can be
/// scoped to their enclosing folder. Absent in pre-v7 files (loads as flat).
/// v8 adds the document's default gradient stops (`gradient`), the persisted
/// setting for the Gradient tool. Absent in pre-v8 files (tool falls back to
/// primary/secondary colours).
pub const SCHEMA_VERSION: u32 = 8;
pub const SUPPORTED_SCHEMA_VERSIONS: &[u32] = &[1, 2, 3, 4, 5, 6, 7, 8];
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Top-level archive metadata written to `manifest.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub app_version: String,
    pub created_at: String,
}

/// One entry in the layer list inside `document.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct LayerEntry {
    /// Stable opaque id - matches the PNG filename in `layers/<id>.png`.
    pub id: String,
    pub name: String,
    pub visible: bool,
    /// Raster, or a component instance. Absent in pre-v3 files (defaults to
    /// `Raster`).
    #[serde(default)]
    pub kind: LayerKind,
    /// Composite blend mode. Absent in pre-v5 files (defaults to `Normal`).
    #[serde(default)]
    pub blend: BlendMode,
    /// Layer opacity in `0.0..=1.0`. Absent in pre-v5 files (defaults to 1.0).
    #[serde(default = "default_opacity")]
    pub opacity: f32,
}

fn default_opacity() -> f32 {
    1.0
}

/// One raster layer inside a component (in `components.json`). Its pixels live
/// at `components/<component id>/layers/<id>.png`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ComponentLayerEntry {
    pub id: String,
    pub name: String,
    pub visible: bool,
    /// Composite blend mode + opacity. Absent in pre-v5 files (Normal / 1.0).
    #[serde(default)]
    pub blend: BlendMode,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
}

/// A component definition written to `components.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ComponentData {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub active_layer: Option<usize>,
    pub layers: Vec<ComponentLayerEntry>,
}

/// Full document description written to `document.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct DocumentData {
    pub canvas_width: u32,
    pub canvas_height: u32,
    pub dpi: f32,
    pub active_layer: Option<usize>,
    pub layers: Vec<LayerEntry>,
    /// Folder structure over `layers` (canvas order, bottom-to-top). Empty or
    /// absent (pre-v7) = flat, no folders.
    #[serde(default)]
    pub layer_tree: Vec<LayerTreeNode>,
    /// Document default gradient stops for the Gradient tool. Absent (pre-v8)
    /// or `None` = derive the ramp from the primary/secondary colours.
    #[serde(default)]
    pub gradient: Option<crate::tools::GradientSettings>,
}

/// The complete in-memory representation of an `.oxiedrawproj` archive.
///
/// Produced by [`super::load::load`] and consumed by [`super::load::apply`];
/// also built by [`super::save::save`] before writing the archive to disk.
pub struct OxieProject {
    pub manifest: Manifest,
    pub document: DocumentData,
    /// Layer pixel data keyed by [`LayerEntry::id`], BGRA8 row-major no padding.
    pub layer_pixels: HashMap<String, Vec<u8>>,
    /// Component definitions (empty for pre-v3 files).
    pub components: Vec<ComponentData>,
    /// Component layer pixels keyed by `"{component_id}/{layer_id}"`, BGRA8.
    pub component_pixels: HashMap<String, Vec<u8>>,
    /// Embedded font metadata (empty for pre-v4 files), from `fonts.json`.
    pub fonts: Vec<FontMeta>,
    /// Embedded font file bytes keyed by content hash (from `fonts/<hash>`).
    pub font_bytes: HashMap<String, Vec<u8>>,
}
