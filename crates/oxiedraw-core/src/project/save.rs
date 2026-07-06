use std::io::{Cursor, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use tar::{Builder, Header};

use crate::canvas::Canvas;
use crate::components::ComponentLibrary;
use crate::document::DocumentProperties;
use crate::text::fonts::{FontMeta, FontRegistry};

use super::ProjectError;
use super::format::{
    APP_VERSION, ComponentData, ComponentLayerEntry, DocumentData, LayerEntry, Manifest,
    SCHEMA_VERSION,
};

/// A self-contained, `Send` snapshot of everything needed to write a project
/// archive. Produced on the main thread by [`snapshot`] (which reads layer
/// pixels back from the GPU); consumed by [`write_snapshot`], which does the
/// CPU-heavy PNG encoding + TAR writing and can run on a worker thread.
pub struct ProjectSnapshot {
    doc: DocumentData,
    canvas_width: u32,
    canvas_height: u32,
    /// `(layer id, BGRA8 pixels)` in z-order.
    layers: Vec<(String, Vec<u8>)>,
    /// Component definitions for `components.json`.
    components: Vec<ComponentData>,
    /// `(component_id, layer_id, width, height, BGRA8 pixels)` for each
    /// component layer.
    component_layers: Vec<(String, String, u32, u32, Vec<u8>)>,
    /// Embedded font metadata for `fonts.json`.
    fonts: Vec<FontMeta>,
    /// `(hash, font-file bytes)` for each embedded font.
    font_bytes: Vec<(String, Vec<u8>)>,
}

/// Collect a [`ProjectSnapshot`] from the canvas. Reads each layer back from the
/// GPU, so it must run on the main thread, but it does no encoding or I/O.
pub fn snapshot(
    canvas: &mut Canvas,
    props: &DocumentProperties,
    components: &ComponentLibrary,
    fonts: &FontRegistry,
    gradient: Option<crate::tools::GradientSettings>,
) -> Result<ProjectSnapshot, ProjectError> {
    let layer_snapshot = canvas.layers().snapshot();
    let canvas_size = canvas.size();
    let active_layer = canvas.layers().active();

    let layers: Vec<LayerEntry> = layer_snapshot
        .iter()
        .map(|l| LayerEntry {
            id: l.id.clone(),
            name: l.name.clone(),
            visible: l.visible,
            kind: l.kind.clone(),
            blend: l.blend,
            opacity: l.opacity,
        })
        .collect();

    let doc = DocumentData {
        canvas_width: canvas_size.width,
        canvas_height: canvas_size.height,
        dpi: props.dpi,
        active_layer,
        layers,
        layer_tree: canvas.layer_tree().to_vec(),
        gradient,
    };

    let mut layer_pixels = Vec::with_capacity(layer_snapshot.len());
    for (idx, layer) in layer_snapshot.iter().enumerate() {
        let pixels = canvas.read_layer(idx)?;
        layer_pixels.push((layer.id.clone(), pixels));
    }

    // Component library: definitions + per-layer pixels (already CPU-side).
    let mut component_data = Vec::with_capacity(components.len());
    let mut component_layers = Vec::new();
    for c in &components.components {
        component_data.push(ComponentData {
            id: c.id.clone(),
            name: c.name.clone(),
            width: c.size.width,
            height: c.size.height,
            active_layer: c.active_layer,
            layers: c
                .layers
                .iter()
                .map(|l| ComponentLayerEntry {
                    id: l.id.clone(),
                    name: l.name.clone(),
                    visible: l.visible,
                    blend: l.blend,
                    opacity: l.opacity,
                })
                .collect(),
        });
        for l in &c.layers {
            component_layers.push((
                c.id.clone(),
                l.id.clone(),
                c.size.width,
                c.size.height,
                l.pixels.clone(),
            ));
        }
    }

    // Embedded fonts: metadata + raw bytes.
    let font_meta = fonts.metadata();
    let font_bytes: Vec<(String, Vec<u8>)> = fonts
        .iter()
        .map(|f| (f.hash.clone(), f.bytes.as_ref().clone()))
        .collect();

    Ok(ProjectSnapshot {
        doc,
        canvas_width: canvas_size.width,
        canvas_height: canvas_size.height,
        layers: layer_pixels,
        components: component_data,
        component_layers,
        fonts: font_meta,
        font_bytes,
    })
}

/// Encode and write a [`ProjectSnapshot`] to an `.oxiedrawproj` archive. Pure
/// data in, file out - safe to call from a worker thread.
///
/// The archive is built into a sibling temp file, flushed to disk, then
/// atomically renamed over `path`. A failed or interrupted write leaves the
/// previous project file untouched instead of truncating it in place.
pub fn write_snapshot(snapshot: &ProjectSnapshot, path: &Path) -> Result<(), ProjectError> {
    let tmp_path = temp_path_for(path);

    // Build into the temp file in its own scope, so any failure returns before
    // we touch the real project file.
    let build = (|| -> Result<(), ProjectError> {
        let file = std::fs::File::create(&tmp_path)?;
        let mut archive = Builder::new(file);
        build_archive(&mut archive, snapshot)?;
        // finish() writes the end-of-archive marker; into_inner() hands the File
        // back so we can fsync before the rename makes the new file visible.
        archive.finish()?;
        archive.into_inner()?.sync_all()?;
        Ok(())
    })();

    if let Err(e) = build {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    // Atomic on the same filesystem: a reader sees either the old file or the
    // new one, never the half-written archive.
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    Ok(())
}

/// Write every archive entry (manifest, document, layers, components, fonts)
/// into `archive`. The first failing entry aborts the whole build.
fn build_archive<W: Write>(
    archive: &mut Builder<W>,
    snapshot: &ProjectSnapshot,
) -> Result<(), ProjectError> {
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        app_version: APP_VERSION.to_string(),
        created_at: utc_timestamp_now(),
    };
    append_json(archive, "manifest.json", &manifest)?;
    append_json(archive, "document.json", &snapshot.doc)?;

    for (id, pixels) in &snapshot.layers {
        let png_bytes = encode_png(pixels, snapshot.canvas_width, snapshot.canvas_height)?;
        append_bytes(archive, &format!("layers/{id}.png"), &png_bytes)?;
    }

    if !snapshot.components.is_empty() {
        append_json(archive, "components.json", &snapshot.components)?;
        for (comp_id, layer_id, w, h, pixels) in &snapshot.component_layers {
            let png_bytes = encode_png(pixels, *w, *h)?;
            append_bytes(
                archive,
                &format!("components/{comp_id}/layers/{layer_id}.png"),
                &png_bytes,
            )?;
        }
    }

    if !snapshot.fonts.is_empty() {
        append_json(archive, "fonts.json", &snapshot.fonts)?;
        for (hash, bytes) in &snapshot.font_bytes {
            append_bytes(archive, &format!("fonts/{hash}"), bytes)?;
        }
    }

    Ok(())
}

/// A sibling temp path in the same directory as `path`, so the later rename
/// stays on one filesystem and is atomic. The pid keeps a second writer from
/// clobbering an in-flight temp file.
fn temp_path_for(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.part", std::process::id()));
    path.with_file_name(name)
}

/// Convenience: snapshot then write synchronously, on the calling thread.
pub fn save(
    canvas: &mut Canvas,
    props: &DocumentProperties,
    components: &ComponentLibrary,
    fonts: &FontRegistry,
    gradient: Option<crate::tools::GradientSettings>,
    path: &Path,
) -> Result<(), ProjectError> {
    let snap = snapshot(canvas, props, components, fonts, gradient)?;
    write_snapshot(&snap, path)
}

fn append_json<W, T>(archive: &mut Builder<W>, name: &str, value: &T) -> Result<(), ProjectError>
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
) -> Result<(), ProjectError> {
    let mut header = Header::new_gnu();
    header.set_size(u64::try_from(bytes.len()).expect("archive entry size fits u64"));
    header.set_mode(0o644);
    header.set_cksum();
    archive.append_data(&mut header, name, Cursor::new(bytes))?;
    Ok(())
}

fn encode_png(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ProjectError> {
    // BGRA -> RGBA: swap B and R channels for PNG storage.
    let rgba: Vec<u8> = bgra
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect();

    let mut out: Vec<u8> = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| ProjectError::Png(e.to_string()))?;
    writer
        .write_image_data(&rgba)
        .map_err(|e| ProjectError::Png(e.to_string()))?;
    drop(writer);
    Ok(out)
}

fn utc_timestamp_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let secs_per_day = 86_400_u64;
    let day_secs = secs % secs_per_day;
    let days = secs / secs_per_day;
    let hour = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let sec = day_secs % 60;
    let (year, month, day) = unix_days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

#[allow(clippy::manual_div_ceil)]
const fn unix_days_to_ymd(days: u64) -> (u64, u64, u64) {
    let jd = days + 2_440_588;
    let l = jd + 68_569;
    let n = 4 * l / 146_097;
    let l = l - (146_097 * n + 3) / 4;
    let i = 4_000 * (l + 1) / 1_461_001;
    let l = l - 1_461 * i / 4 + 31;
    let j = 80 * l / 2_447;
    let day = l - 2_447 * j / 80;
    let l = j / 11;
    let month = j + 2 - 12 * l;
    let year = 100 * (n - 49) + i + l;
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::format::DocumentData;

    /// A snapshot whose single layer carries a wrong-sized pixel buffer, so
    /// `encode_png` fails partway through the archive build - the exact shape of
    /// the failure that once truncated projects to a headerless stub.
    fn failing_snapshot() -> ProjectSnapshot {
        let doc = DocumentData {
            canvas_width: 4,
            canvas_height: 4,
            dpi: 96.0,
            active_layer: Some(0),
            layers: Vec::new(),
            layer_tree: Vec::new(),
            gradient: None,
        };
        ProjectSnapshot {
            doc,
            canvas_width: 4,
            canvas_height: 4,
            // A 4x4 layer needs 64 bytes; hand it 3 so PNG encoding rejects it.
            layers: vec![("0000000000000004".to_string(), vec![0u8; 3])],
            components: Vec::new(),
            component_layers: Vec::new(),
            fonts: Vec::new(),
            font_bytes: Vec::new(),
        }
    }

    // A failed write must leave the previous project file byte-for-byte intact
    // and must not leave a temp file behind.
    #[test]
    fn failed_write_preserves_existing_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oxiedraw_atomic_{}.oxiedrawproj", std::process::id()));
        let original = b"PREVIOUS GOOD PROJECT".to_vec();
        std::fs::write(&path, &original).expect("seed existing file");

        let err = write_snapshot(&failing_snapshot(), &path);
        assert!(err.is_err(), "encoding a bad layer must fail the write");

        let after = std::fs::read(&path).expect("original file must still exist");
        assert_eq!(after, original, "a failed save must not touch the old file");
        assert!(
            !temp_path_for(&path).exists(),
            "the temp file must be cleaned up after a failed write"
        );

        std::fs::remove_file(&path).ok();
    }
}
