use std::io::{Cursor, Write};
use std::path::Path;
use std::rc::Rc;

use tar::{Builder, Header};

use super::super::pattern::PatternData;
use super::super::preset::{BrushFamily, BrushPreset};
use super::error::BrushError;
use super::types::{
    APP_VERSION, BUILTIN_REVISION, BrushDocument, BrushManifest, FamilyDoc, KIND, SCHEMA_VERSION,
};

/// Write `preset` to an `.oxiebrush` archive at `path`.
pub fn save(preset: &BrushPreset, path: &Path) -> Result<(), BrushError> {
    let file = std::fs::File::create(path)?;
    let mut archive = Builder::new(file);

    let manifest = BrushManifest {
        schema_version: SCHEMA_VERSION,
        app_version: APP_VERSION.to_string(),
        kind: KIND.to_string(),
        name: preset.name.clone(),
        builtin_revision: BUILTIN_REVISION,
    };
    append_json(&mut archive, "manifest.json", &manifest)?;

    // Each `(filename, PatternData)` is written into `patterns/`.
    let mut pattern_payloads: Vec<(String, Rc<PatternData>)> = Vec::new();
    let family_doc = match &preset.family {
        BrushFamily::SoftRound => FamilyDoc::SoftRound,
        BrushFamily::Pixel => FamilyDoc::Pixel,
        BrushFamily::Smudge => FamilyDoc::Smudge,
        BrushFamily::Textured(data) => {
            let filename = "pattern.png".to_string();
            pattern_payloads.push((filename.clone(), data.clone()));
            FamilyDoc::Textured { pattern: filename }
        }
        BrushFamily::ImageTip { tip, grain } => {
            let tip_name = "tip.png".to_string();
            pattern_payloads.push((tip_name.clone(), tip.clone()));
            let grain_name = grain.as_ref().map(|g| {
                let name = "grain.png".to_string();
                pattern_payloads.push((name.clone(), g.clone()));
                name
            });
            FamilyDoc::ImageTip {
                tip: tip_name,
                grain: grain_name,
            }
        }
    };

    let document = BrushDocument {
        family: family_doc,
        default_size: preset.default_size,
        default_opacity: preset.default_opacity,
        spacing_ratio: preset.spacing_ratio,
        stabilizer: preset.stabilizer,
        speed_smoothing: preset.speed_smoothing,
        buildup: preset.buildup,
        hardness: preset.hardness,
        tip: preset.tip,
        texture_scale: preset.texture_scale,
        texture_strength: preset.texture_strength,
        texturing_mode: preset.texturing_mode,
        dynamics: preset.dynamics.clone(),
    };
    append_json(&mut archive, "brush.json", &document)?;

    for (filename, data) in &pattern_payloads {
        let png_bytes = encode_premul_rgba_to_png(data)?;
        append_bytes(&mut archive, &format!("patterns/{filename}"), &png_bytes)?;
    }

    if let Some(icon_bytes) = &preset.icon {
        append_bytes(&mut archive, "icon.png", icon_bytes)?;
    }

    if let Some(preview_bytes) = &preset.preview {
        append_bytes(&mut archive, "preview.png", preview_bytes)?;
    }

    archive.finish()?;
    Ok(())
}

fn append_json<W, T>(archive: &mut Builder<W>, name: &str, value: &T) -> Result<(), BrushError>
where
    W: Write,
    T: serde::Serialize,
{
    let json = serde_json::to_string_pretty(value)?;
    append_bytes(archive, name, json.as_bytes())
}

fn append_bytes<W: Write>(
    archive: &mut Builder<W>,
    name: &str,
    bytes: &[u8],
) -> Result<(), BrushError> {
    let mut header = Header::new_gnu();
    header.set_size(u64::try_from(bytes.len()).expect("archive entry size fits u64"));
    header.set_mode(0o644);
    header.set_cksum();
    archive.append_data(&mut header, name, Cursor::new(bytes))?;
    Ok(())
}

/// Encode premultiplied RGBA pattern bytes back to a plain PNG. We
/// undo premultiplication so the PNG file stores conventional
/// straight-alpha colours (what every other tool expects).
fn encode_premul_rgba_to_png(data: &Rc<PatternData>) -> Result<Vec<u8>, BrushError> {
    let mut straight = Vec::with_capacity(data.rgba.len());
    for chunk in data.rgba.chunks_exact(4) {
        let a = chunk[3];
        if a == 0 {
            straight.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let inv = 255.0 / f32::from(a);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                straight.push((f32::from(chunk[0]) * inv).min(255.0) as u8);
                straight.push((f32::from(chunk[1]) * inv).min(255.0) as u8);
                straight.push((f32::from(chunk[2]) * inv).min(255.0) as u8);
                straight.push(a);
            }
        }
    }

    let mut out: Vec<u8> = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, data.width, data.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| BrushError::Png(e.to_string()))?;
    writer
        .write_image_data(&straight)
        .map_err(|e| BrushError::Png(e.to_string()))?;
    drop(writer);
    Ok(out)
}
