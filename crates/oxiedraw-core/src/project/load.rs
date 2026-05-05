use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use crate::canvas::Canvas;
use crate::components::{Component, ComponentLayer, ComponentLibrary};

use super::ProjectError;
use super::format::{
    ComponentData, DocumentData, Manifest, OxieProject, SCHEMA_VERSION, SUPPORTED_SCHEMA_VERSIONS,
};
use oxiedraw_utils::geometry::Size;

/// Parse an `.oxiedrawproj` archive from `path` into an [`OxieProject`].
pub fn load(path: &Path) -> Result<OxieProject, ProjectError> {
    let file = std::fs::File::open(path)?;
    let mut archive = tar::Archive::new(file);

    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut document_bytes: Option<Vec<u8>> = None;
    let mut components_bytes: Option<Vec<u8>> = None;
    let mut fonts_bytes: Option<Vec<u8>> = None;
    let mut layer_pngs: HashMap<String, Vec<u8>> = HashMap::new();
    // Component layer PNGs keyed by "{component_id}/{layer_id}".
    let mut component_pngs: HashMap<String, Vec<u8>> = HashMap::new();
    // Embedded font files keyed by content hash.
    let mut font_files: HashMap<String, Vec<u8>> = HashMap::new();

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let entry_path = entry.path()?.to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;

        if entry_path == "manifest.json" {
            manifest_bytes = Some(bytes);
        } else if entry_path == "document.json" {
            document_bytes = Some(bytes);
        } else if entry_path == "components.json" {
            components_bytes = Some(bytes);
        } else if entry_path == "fonts.json" {
            fonts_bytes = Some(bytes);
        } else if let Some(hash) = entry_path.strip_prefix("fonts/").filter(|h| !h.is_empty()) {
            font_files.insert(hash.to_string(), bytes);
        } else if let Some(stem) = entry_path
            .strip_prefix("layers/")
            .and_then(|s| s.strip_suffix(".png"))
        {
            layer_pngs.insert(stem.to_string(), bytes);
        } else if let Some(rest) = entry_path
            .strip_prefix("components/")
            .and_then(|s| s.strip_suffix(".png"))
        {
            // rest = "{component_id}/layers/{layer_id}"
            if let Some((cid, lid)) = rest.split_once("/layers/") {
                component_pngs.insert(format!("{cid}/{lid}"), bytes);
            }
        }
    }

    let manifest: Manifest = serde_json::from_slice(
        manifest_bytes
            .as_deref()
            .ok_or_else(|| ProjectError::MissingEntry("manifest.json".to_string()))?,
    )?;

    if !SUPPORTED_SCHEMA_VERSIONS.contains(&manifest.schema_version) {
        return Err(ProjectError::UnsupportedSchema {
            found: manifest.schema_version,
            expected: SCHEMA_VERSION,
        });
    }

    let document: DocumentData = serde_json::from_slice(
        document_bytes
            .as_deref()
            .ok_or_else(|| ProjectError::MissingEntry("document.json".to_string()))?,
    )?;

    let mut layer_pixels = HashMap::new();
    for layer in &document.layers {
        let png_bytes = layer_pngs
            .get(&layer.id)
            .ok_or_else(|| ProjectError::MissingEntry(format!("layers/{}.png", layer.id)))?;
        let pixels = decode_png(png_bytes, document.canvas_width, document.canvas_height)?;
        layer_pixels.insert(layer.id.clone(), pixels);
    }

    // Components (absent in pre-v3 files).
    let components: Vec<ComponentData> = match components_bytes {
        Some(bytes) => serde_json::from_slice(&bytes)?,
        None => Vec::new(),
    };
    let mut component_pixels = HashMap::new();
    for comp in &components {
        for layer in &comp.layers {
            let key = format!("{}/{}", comp.id, layer.id);
            let png_bytes = component_pngs
                .get(&key)
                .ok_or_else(|| ProjectError::MissingEntry(format!("components/{key}.png")))?;
            let pixels = decode_png(png_bytes, comp.width, comp.height)?;
            component_pixels.insert(key, pixels);
        }
    }

    // Embedded fonts (absent in pre-v4 files).
    let fonts: Vec<crate::text::fonts::FontMeta> = match fonts_bytes {
        Some(bytes) => serde_json::from_slice(&bytes)?,
        None => Vec::new(),
    };

    Ok(OxieProject {
        manifest,
        document,
        layer_pixels,
        components,
        component_pixels,
        fonts,
        font_bytes: font_files,
    })
}

/// Build a [`ComponentLibrary`] from a loaded project (empty for pre-v3 files).
#[must_use]
pub fn build_components(project: &OxieProject) -> ComponentLibrary {
    let mut lib = ComponentLibrary::new();
    for comp in &project.components {
        let layers: Vec<ComponentLayer> = comp
            .layers
            .iter()
            .map(|l| {
                let key = format!("{}/{}", comp.id, l.id);
                ComponentLayer {
                    id: l.id.clone(),
                    name: l.name.clone(),
                    visible: l.visible,
                    pixels: project.component_pixels.get(&key).cloned().unwrap_or_default(),
                }
            })
            .collect();
        lib.push(Component::from_parts(
            comp.id.clone(),
            comp.name.clone(),
            Size::new(comp.width, comp.height),
            layers,
            comp.active_layer,
        ));
    }
    lib
}

/// Apply a loaded project to `canvas`, replacing all layers and pixel data.
///
/// Fails if the project canvas size does not match or the project has no layers.
pub fn apply(project: &OxieProject, canvas: &mut Canvas) -> Result<(), ProjectError> {
    let canvas_size = canvas.size();
    let doc = &project.document;

    if doc.canvas_width != canvas_size.width || doc.canvas_height != canvas_size.height {
        return Err(ProjectError::CanvasSizeMismatch {
            proj_w: doc.canvas_width,
            proj_h: doc.canvas_height,
            cur_w: canvas_size.width,
            cur_h: canvas_size.height,
        });
    }

    if doc.layers.is_empty() {
        return Err(ProjectError::NoLayers);
    }

    let layers: Vec<(String, String, bool, Vec<u8>)> = doc
        .layers
        .iter()
        .map(|entry| {
            let pixels = project
                .layer_pixels
                .get(&entry.id)
                .cloned()
                .unwrap_or_default();
            (entry.id.clone(), entry.name.clone(), entry.visible, pixels)
        })
        .collect();

    canvas.replace_all_layers(&layers)?;

    // Restore layer kinds (component instances). replace_all_layers resets
    // everything to Raster, so re-apply the saved kinds by index.
    for (idx, entry) in doc.layers.iter().enumerate() {
        if entry.kind != crate::document::LayerKind::Raster {
            canvas.layers().set_kind(idx, entry.kind.clone());
        }
    }

    if let Some(active) = doc.active_layer
        && active < canvas.layers().len()
    {
        canvas.layers().set_active(Some(active));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::format::{
        ComponentLayerEntry, DocumentData, Manifest, APP_VERSION, SCHEMA_VERSION,
    };

    #[test]
    fn build_components_reconstructs_library() {
        let pixels = vec![9u8; 2 * 2 * 4];
        let project = OxieProject {
            manifest: Manifest {
                schema_version: SCHEMA_VERSION,
                app_version: APP_VERSION.to_string(),
                created_at: String::new(),
            },
            document: DocumentData {
                canvas_width: 2,
                canvas_height: 2,
                dpi: 96.0,
                active_layer: None,
                layers: Vec::new(),
            },
            layer_pixels: HashMap::new(),
            components: vec![ComponentData {
                id: "c1".to_string(),
                name: "Star".to_string(),
                width: 2,
                height: 2,
                active_layer: Some(0),
                layers: vec![ComponentLayerEntry {
                    id: "l1".to_string(),
                    name: "L1".to_string(),
                    visible: true,
                }],
            }],
            component_pixels: HashMap::from([("c1/l1".to_string(), pixels.clone())]),
            fonts: Vec::new(),
            font_bytes: HashMap::new(),
        };

        let lib = build_components(&project);
        assert_eq!(lib.len(), 1);
        let c = lib.get("c1").expect("component present");
        assert_eq!(c.name, "Star");
        assert_eq!(c.size, Size::new(2, 2));
        assert_eq!(c.active_layer, Some(0));
        assert_eq!(c.layers.len(), 1);
        assert_eq!(c.layers[0].pixels, pixels);
        // Master is the flattened render; a single opaque layer reproduces it.
        assert_eq!(c.master, pixels);
    }

    /// A v4 archive's `fonts.json` + `fonts/<hash>` entries load into the
    /// project's font metadata and byte map. No GPU needed (load, not apply).
    #[test]
    fn load_reads_embedded_fonts() {
        use std::io::Cursor;

        let path = std::env::temp_dir().join("oxiedraw_embedded_fonts_test.oxiedrawproj");
        {
            let file = std::fs::File::create(&path).expect("create");
            let mut ar = tar::Builder::new(file);
            let mut append = |name: &str, bytes: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(bytes.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                ar.append_data(&mut h, name, Cursor::new(bytes.to_vec()))
                    .expect("append");
            };
            append(
                "manifest.json",
                br#"{"schema_version":4,"app_version":"t","created_at":""}"#,
            );
            append(
                "document.json",
                br#"{"canvas_width":1,"canvas_height":1,"dpi":96.0,"active_layer":null,"layers":[]}"#,
            );
            append("fonts.json", br#"[{"hash":"abc123","families":["Foo"]}]"#);
            append("fonts/abc123", b"FONTDATA");
            ar.finish().expect("finish");
        }

        let project = load(&path).expect("load");
        assert_eq!(project.fonts.len(), 1);
        assert_eq!(project.fonts[0].families, vec!["Foo".to_string()]);
        assert_eq!(
            project.font_bytes.get("abc123").map(Vec::as_slice),
            Some(b"FONTDATA".as_slice())
        );
        std::fs::remove_file(&path).ok();
    }
}

fn decode_png(bytes: &[u8], expected_w: u32, expected_h: u32) -> Result<Vec<u8>, ProjectError> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder
        .read_info()
        .map_err(|e| ProjectError::Png(e.to_string()))?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| ProjectError::Png(e.to_string()))?;

    if info.width != expected_w || info.height != expected_h {
        return Err(ProjectError::Png(format!(
            "PNG size {}x{} doesn't match expected {}x{}",
            info.width, info.height, expected_w, expected_h
        )));
    }

    let rgba = &buf[..info.buffer_size()];
    // RGBA -> BGRA: swap R and B to match the Vulkan layer image byte order.
    let bgra: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect();
    Ok(bgra)
}
