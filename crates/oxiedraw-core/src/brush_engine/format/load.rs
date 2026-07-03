use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::super::brush::BrushPresetId;
use super::super::pattern::PatternData;
use super::super::preset::{BrushFamily, BrushPreset};
use super::error::BrushError;
use super::types::{
    BrushDocument, BrushManifest, BrushPackage, FamilyDoc, KIND, SCHEMA_VERSION,
};

/// Parse an `.oxiebrush` archive from disk. The archive is fully read
/// into memory - brush files are small (a few KB to ~1 MB with a
/// pattern), so streaming is not worth the complexity.
pub fn load(path: &Path) -> Result<BrushPackage, BrushError> {
    let file = std::fs::File::open(path)?;
    let mut archive = tar::Archive::new(file);

    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut document_bytes: Option<Vec<u8>> = None;
    let mut pattern_pngs: HashMap<String, Vec<u8>> = HashMap::new();
    let mut icon_bytes: Option<Vec<u8>> = None;
    let mut preview_bytes: Option<Vec<u8>> = None;

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let entry_path = entry.path()?.to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;

        if entry_path == "manifest.json" {
            manifest_bytes = Some(bytes);
        } else if entry_path == "brush.json" {
            document_bytes = Some(bytes);
        } else if entry_path == "icon.png" {
            icon_bytes = Some(bytes);
        } else if entry_path == "preview.png" {
            preview_bytes = Some(bytes);
        } else if let Some(filename) = entry_path.strip_prefix("patterns/") {
            pattern_pngs.insert(filename.to_string(), bytes);
        }
    }

    let manifest: BrushManifest = serde_json::from_slice(
        manifest_bytes
            .as_deref()
            .ok_or_else(|| BrushError::MissingEntry("manifest.json".to_string()))?,
    )?;
    if manifest.kind != KIND {
        return Err(BrushError::NotABrush(manifest.kind));
    }
    // Older archives load fine: new `brush.json` fields are
    // `#[serde(default)]`. Only reject archives from a *newer* schema we
    // can't understand.
    if manifest.schema_version > SCHEMA_VERSION {
        return Err(BrushError::UnsupportedSchema {
            found: manifest.schema_version,
            expected: SCHEMA_VERSION,
        });
    }

    let document: BrushDocument = serde_json::from_slice(
        document_bytes
            .as_deref()
            .ok_or_else(|| BrushError::MissingEntry("brush.json".to_string()))?,
    )?;

    let mut patterns = HashMap::new();
    for (filename, png_bytes) in pattern_pngs {
        let (rgba, w, h) = decode_png_to_premul_rgba(&png_bytes)?;
        patterns.insert(filename, (rgba, w, h));
    }

    Ok(BrushPackage {
        manifest,
        document,
        patterns,
        icon: icon_bytes,
        preview: preview_bytes,
    })
}

impl BrushPackage {
    /// Convert the parsed archive into a runtime `BrushPreset`.
    /// `source_path` records where the archive came from so the brush
    /// manager can save edits back to the same file. Errors when a
    /// textured brush's pattern filename can't be resolved against the
    /// archive's patterns map.
    pub fn into_preset(
        self,
        id: BrushPresetId,
        source_path: Option<PathBuf>,
    ) -> Result<BrushPreset, BrushError> {
        let family = match self.document.family {
            FamilyDoc::SoftRound => BrushFamily::SoftRound,
            FamilyDoc::Pixel => BrushFamily::Pixel,
            FamilyDoc::Textured { pattern } => {
                let (rgba, w, h) = self
                    .patterns
                    .get(&pattern)
                    .ok_or_else(|| BrushError::MissingPattern(pattern.clone()))?;
                let data = PatternData::new(rgba.clone(), *w, *h);
                BrushFamily::Textured(Rc::new(data))
            }
        };
        Ok(BrushPreset {
            id,
            name: self.manifest.name,
            family,
            default_size: self.document.default_size,
            default_opacity: self.document.default_opacity,
            spacing_ratio: self.document.spacing_ratio,
            stabilizer: self.document.stabilizer,
            speed_smoothing: self.document.speed_smoothing,
            buildup: self.document.buildup,
            hardness: self.document.hardness,
            tip: self.document.tip,
            texture_scale: self.document.texture_scale,
            texture_strength: self.document.texture_strength,
            dynamics: self.document.dynamics,
            icon: self.icon,
            preview: self.preview,
            source_path,
        })
    }
}

/// Decode a PNG into premultiplied RGBA8. PNGs by convention carry
/// straight (non-premultiplied) alpha; the atlas expects premul, so
/// we premultiply on the way in.
fn decode_png_to_premul_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), BrushError> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder
        .read_info()
        .map_err(|e| BrushError::Png(e.to_string()))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| BrushError::Png(e.to_string()))?;

    let straight = &buf[..info.buffer_size()];
    let mut premul = Vec::with_capacity(straight.len());
    match info.color_type {
        png::ColorType::Rgba => {
            for chunk in straight.chunks_exact(4) {
                let a = chunk[3];
                let f = f32::from(a) / 255.0;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    premul.push((f32::from(chunk[0]) * f).round() as u8);
                    premul.push((f32::from(chunk[1]) * f).round() as u8);
                    premul.push((f32::from(chunk[2]) * f).round() as u8);
                    premul.push(a);
                }
            }
        }
        png::ColorType::Rgb => {
            for chunk in straight.chunks_exact(3) {
                premul.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xFF]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for chunk in straight.chunks_exact(2) {
                let v = chunk[0];
                let a = chunk[1];
                let f = f32::from(a) / 255.0;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let p = (f32::from(v) * f).round() as u8;
                premul.extend_from_slice(&[p, p, p, a]);
            }
        }
        png::ColorType::Grayscale => {
            for &v in straight {
                premul.extend_from_slice(&[v, v, v, 0xFF]);
            }
        }
        other @ png::ColorType::Indexed => {
            return Err(BrushError::Png(format!(
                "unsupported PNG color type {other:?}"
            )));
        }
    }
    Ok((premul, info.width, info.height))
}
