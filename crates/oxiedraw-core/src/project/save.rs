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
    view_rotation: f32,
    guide: Option<crate::guides::GuideConfig>,
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
        view_rotation,
        guide,
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

/// Write a [`ProjectSnapshot`] to `path` as an `.oxiedrawproj` archive. Safe to
/// call off the main thread. Builds into a temp sibling and atomically renames
/// over `path`, so a failed write never truncates the old file. `backup_count`
/// > 0 first rotates the previous file into `<path>-1`..`-N` (`-N` newest); 0
/// overwrites without backups.
pub fn write_snapshot(
    snapshot: &ProjectSnapshot,
    path: &Path,
    backup_count: usize,
) -> Result<(), ProjectError> {
    let tmp_path = temp_path_for(path);

    // Fsync the temp file before it is renamed into place.
    let build = (|| -> Result<(), ProjectError> {
        let file = std::fs::File::create(&tmp_path)?;
        let mut archive = Builder::new(file);
        build_archive(&mut archive, snapshot)?;
        archive.finish()?;
        archive.into_inner()?.sync_all()?;
        Ok(())
    })();

    if let Err(e) = build {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    // A rotation hiccup must not abort the save, so log rather than propagate.
    if backup_count > 0
        && path.exists()
        && let Err(e) = rotate_backups(path, backup_count)
    {
        tracing::warn!(path = %path.display(), err = %e, "backup rotation failed");
    }

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    Ok(())
}

/// The path of numbered backup `slot` for `main` (e.g. `foo.oxiedrawproj-2`).
fn backup_path(main: &Path, slot: usize) -> std::path::PathBuf {
    let mut name = main.file_name().unwrap_or_default().to_os_string();
    name.push(format!("-{slot}"));
    main.with_file_name(name)
}

/// Roll `main` into the newest backup slot (`-count`): drop the oldest, shift
/// the rest down, move `main` up. Slots above `count` (from a since-lowered
/// setting) are pruned.
fn rotate_backups(main: &Path, count: usize) -> std::io::Result<()> {
    let mut slot = count + 1;
    while backup_path(main, slot).exists() {
        let _ = std::fs::remove_file(backup_path(main, slot));
        slot += 1;
    }
    let _ = std::fs::remove_file(backup_path(main, 1));
    // -2 -> -1, -3 -> -2, ...
    for slot in 1..count {
        let from = backup_path(main, slot + 1);
        if from.exists() {
            std::fs::rename(&from, backup_path(main, slot))?;
        }
    }
    std::fs::rename(main, backup_path(main, count))
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

/// Sibling temp path next to `path` (same filesystem, so the rename is atomic).
/// The pid avoids two writers sharing one temp file.
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
    view_rotation: f32,
    guide: Option<crate::guides::GuideConfig>,
    path: &Path,
) -> Result<(), ProjectError> {
    let snap = snapshot(canvas, props, components, fonts, gradient, view_rotation, guide)?;
    write_snapshot(&snap, path, 0)
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::document::{BlendMode, LayerKind};
    use crate::project::format::{DocumentData, LayerEntry};
    use crate::project::load;

    // -- Fixtures --------------------------------------------------------------

    /// Unique per-call temp path so parallel tests never collide.
    fn unique_main(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "oxiedraw_test_{}_{n}_{tag}.oxiedrawproj",
            std::process::id()
        ))
    }

    /// Remove the main file, its temp sibling, and every numbered backup.
    fn cleanup(main: &Path) {
        std::fs::remove_file(main).ok();
        std::fs::remove_file(temp_path_for(main)).ok();
        for slot in 1..=12 {
            std::fs::remove_file(backup_path(main, slot)).ok();
        }
    }

    /// A snapshot with a wrong-sized layer buffer, so PNG encoding fails partway
    /// through the build (the failure mode that once truncated saved files).
    fn failing_snapshot() -> ProjectSnapshot {
        ProjectSnapshot {
            doc: doc_meta(4, 4, &[]),
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

    fn doc_meta(width: u32, height: u32, ids: &[&str]) -> DocumentData {
        DocumentData {
            canvas_width: width,
            canvas_height: height,
            dpi: 96.0,
            active_layer: (!ids.is_empty()).then_some(0),
            layers: ids
                .iter()
                .map(|id| LayerEntry {
                    id: (*id).to_string(),
                    name: format!("layer {id}"),
                    visible: true,
                    kind: LayerKind::default(),
                    blend: BlendMode::default(),
                    opacity: 1.0,
                })
                .collect(),
            layer_tree: Vec::new(),
            gradient: None,
            view_rotation: 0.0,
            guide: None,
        }
    }

    /// A valid, encodable snapshot: each id gets a `width x height` layer filled
    /// with a distinct byte so a round-trip can prove pixels land in the right
    /// layer.
    fn valid_snapshot(width: u32, height: u32, ids: &[&str]) -> ProjectSnapshot {
        let px_len = (width * height * 4) as usize;
        let layers = ids
            .iter()
            .enumerate()
            .map(|(i, id)| ((*id).to_string(), vec![fill_byte(i); px_len]))
            .collect();
        ProjectSnapshot {
            doc: doc_meta(width, height, ids),
            canvas_width: width,
            canvas_height: height,
            layers,
            components: Vec::new(),
            component_layers: Vec::new(),
            fonts: Vec::new(),
            font_bytes: Vec::new(),
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn fill_byte(layer_index: usize) -> u8 {
        (10 + layer_index * 7) as u8
    }

    // -- backup_path / temp_path_for ------------------------------------------

    #[test]
    fn backup_path_appends_numeric_suffix() {
        let main = Path::new("/tmp/art.oxiedrawproj");
        assert_eq!(backup_path(main, 1), Path::new("/tmp/art.oxiedrawproj-1"));
        assert_eq!(backup_path(main, 3), Path::new("/tmp/art.oxiedrawproj-3"));
    }

    #[test]
    fn temp_path_is_a_sibling_with_part_suffix() {
        let main = Path::new("/tmp/sub/art.oxiedrawproj");
        let tmp = temp_path_for(main);
        assert_eq!(tmp.parent(), main.parent(), "temp must sit next to the file");
        assert!(
            tmp.file_name().expect("test io").to_string_lossy().ends_with(".part"),
            "temp name must end in .part"
        );
    }

    // -- rotate_backups edge cases --------------------------------------------

    #[test]
    fn rotate_backups_with_no_existing_backups_parks_main_at_top() {
        let main = unique_main("rot_none");
        std::fs::write(&main, b"MAIN").expect("test io");

        rotate_backups(&main, 3).expect("rotate");

        assert!(!main.exists());
        assert!(!backup_path(&main, 1).exists());
        assert!(!backup_path(&main, 2).exists());
        assert_eq!(std::fs::read(backup_path(&main, 3)).expect("test io"), b"MAIN");
        cleanup(&main);
    }

    #[test]
    fn rotate_backups_count_one_keeps_only_newest() {
        let main = unique_main("rot_one");
        std::fs::write(&main, b"MAIN").expect("test io");
        std::fs::write(backup_path(&main, 1), b"OLD").expect("test io");

        rotate_backups(&main, 1).expect("rotate");

        assert!(!main.exists());
        assert_eq!(std::fs::read(backup_path(&main, 1)).expect("test io"), b"MAIN");
        assert!(!backup_path(&main, 2).exists());
        cleanup(&main);
    }

    // A full set: oldest drops, the rest shift down, main takes the newest slot.
    #[test]
    fn rotate_backups_shifts_and_caps() {
        let main = unique_main("rot_full");
        std::fs::write(&main, b"MAIN").expect("test io");
        std::fs::write(backup_path(&main, 1), b"OLD1").expect("test io");
        std::fs::write(backup_path(&main, 2), b"MID2").expect("test io");
        std::fs::write(backup_path(&main, 3), b"NEW3").expect("test io");

        rotate_backups(&main, 3).expect("rotate");

        assert!(!main.exists(), "main must move into the newest backup slot");
        assert_eq!(std::fs::read(backup_path(&main, 1)).expect("test io"), b"MID2");
        assert_eq!(std::fs::read(backup_path(&main, 2)).expect("test io"), b"NEW3");
        assert_eq!(std::fs::read(backup_path(&main, 3)).expect("test io"), b"MAIN");
        cleanup(&main);
    }

    // Lowering the backup count removes the now-stale higher-numbered backups.
    #[test]
    fn rotate_backups_prunes_stale_slots() {
        let main = unique_main("rot_prune");
        std::fs::write(&main, b"MAIN").expect("test io");
        for slot in 1..=5 {
            std::fs::write(backup_path(&main, slot), b"x").expect("test io");
        }

        rotate_backups(&main, 2).expect("rotate");

        assert!(backup_path(&main, 1).exists());
        assert!(backup_path(&main, 2).exists());
        for slot in 3..=5 {
            assert!(!backup_path(&main, slot).exists(), "slot {slot} must be pruned");
        }
        cleanup(&main);
    }

    // -- write_snapshot: archive validity + atomicity -------------------------

    #[test]
    fn write_produces_a_loadable_archive() {
        let main = unique_main("loadable");
        write_snapshot(&valid_snapshot(4, 4, &["a1", "b2"]), &main, 3).expect("write");

        let project = load::load(&main).expect("archive must load");
        assert_eq!(project.document.layers.len(), 2);
        assert_eq!(project.manifest.schema_version, SCHEMA_VERSION);
        assert!(!temp_path_for(&main).exists(), "temp must be gone after success");
        cleanup(&main);
    }

    #[test]
    fn roundtrip_preserves_each_layers_pixels() {
        let main = unique_main("pixels");
        let ids = ["l0", "l1", "l2"];
        write_snapshot(&valid_snapshot(3, 2, &ids), &main, 0).expect("write");

        let project = load::load(&main).expect("load");
        let px_len = 3 * 2 * 4;
        for (i, id) in ids.iter().enumerate() {
            let pixels = project.layer_pixels.get(*id).expect("layer pixels present");
            assert_eq!(pixels.as_slice(), vec![fill_byte(i); px_len].as_slice());
        }
        cleanup(&main);
    }

    #[test]
    fn first_save_creates_no_backup_files() {
        let main = unique_main("first");
        write_snapshot(&valid_snapshot(2, 2, &["x"]), &main, 3).expect("write");

        assert!(main.exists());
        for slot in 1..=3 {
            assert!(!backup_path(&main, slot).exists(), "no backup on first save");
        }
        cleanup(&main);
    }

    #[test]
    fn second_save_moves_previous_main_into_newest_backup() {
        let main = unique_main("second");
        write_snapshot(&valid_snapshot(2, 2, &["v0"]), &main, 3).expect("first write");
        write_snapshot(&valid_snapshot(2, 2, &["v1"]), &main, 3).expect("second write");

        // Newest backup is the previous main - it must load and contain v0.
        let backup = load::load(&backup_path(&main, 3)).expect("newest backup loads");
        assert_eq!(backup.document.layers[0].id, "v0");
        let current = load::load(&main).expect("current loads");
        assert_eq!(current.document.layers[0].id, "v1");
        cleanup(&main);
    }

    // Backups grow from the top slot downward and never exceed `count`; every
    // surviving backup is a real, loadable archive.
    #[test]
    fn repeated_saves_grow_then_cap_at_count() {
        let main = unique_main("grow");
        let count = 3;
        for i in 0..6 {
            let id = format!("gen{i}");
            write_snapshot(&valid_snapshot(2, 2, &[&id]), &main, count).expect("write");
        }

        // Exactly `count` backups, in the top slots, all loadable.
        for slot in 1..=count {
            assert!(backup_path(&main, slot).exists(), "slot {slot} present");
            load::load(&backup_path(&main, slot)).expect("backup loads");
        }
        assert!(!backup_path(&main, count + 1).exists(), "must cap at count");

        // Newest backup (-3) holds the generation just before the current file.
        let newest = load::load(&backup_path(&main, count)).expect("newest loads");
        assert_eq!(newest.document.layers[0].id, "gen4");
        let current = load::load(&main).expect("current loads");
        assert_eq!(current.document.layers[0].id, "gen5");
        cleanup(&main);
    }

    #[test]
    fn backup_count_zero_never_creates_backups() {
        let main = unique_main("zero");
        write_snapshot(&valid_snapshot(2, 2, &["a"]), &main, 0).expect("first");
        write_snapshot(&valid_snapshot(2, 2, &["b"]), &main, 0).expect("second");

        assert!(main.exists());
        assert!(!backup_path(&main, 1).exists(), "count 0 makes no backups");
        load::load(&main).expect("main still valid");
        cleanup(&main);
    }

    // A failed write must leave the previous project file byte-for-byte intact
    // and must not leave a temp file behind.
    #[test]
    fn failed_write_preserves_existing_file() {
        let main = unique_main("fail_main");
        let original = b"PREVIOUS GOOD PROJECT".to_vec();
        std::fs::write(&main, &original).expect("test io");

        let err = write_snapshot(&failing_snapshot(), &main, 3);
        assert!(err.is_err(), "encoding a bad layer must fail the write");

        assert_eq!(std::fs::read(&main).expect("test io"), original, "old file untouched");
        assert!(!temp_path_for(&main).exists(), "temp cleaned up");
        cleanup(&main);
    }

    // A failed write happens before rotation, so existing backups are untouched
    // too - a bad save must not disturb the backup history.
    #[test]
    fn failed_write_does_not_rotate_backups() {
        let main = unique_main("fail_backups");
        std::fs::write(&main, b"MAIN").expect("test io");
        std::fs::write(backup_path(&main, 3), b"NEWEST").expect("test io");

        let err = write_snapshot(&failing_snapshot(), &main, 3);
        assert!(err.is_err());

        assert_eq!(std::fs::read(&main).expect("test io"), b"MAIN", "main untouched");
        assert_eq!(std::fs::read(backup_path(&main, 3)).expect("test io"), b"NEWEST");
        assert!(!backup_path(&main, 2).exists(), "no rotation on failure");
        cleanup(&main);
    }
}
