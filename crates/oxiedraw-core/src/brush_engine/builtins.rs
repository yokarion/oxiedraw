//! Built-in brush presets and disk-seeding helpers.
//!
//! `seed_missing(dir)` walks the canonical filename list and writes any
//! `.oxiebrush` archive that isn't already there. Called on first
//! launch (when the brushes directory is empty) and exposed as the
//! "Repopulate built-in brushes" operation for the future UI.

use std::path::{Path, PathBuf};

use super::brush::BrushPresetId;
use super::format::{self, BrushError};
use super::preset::BrushPreset;

type Factory = fn(BrushPresetId) -> BrushPreset;

/// Id passed into the factories when seeding to disk. The file format
/// drops the id (it's session-local), so any value works here.
const PLACEHOLDER_ID: BrushPresetId = BrushPresetId(0);

/// `(filename, factory)` pairs. Filename is stable across releases so
/// `seed_missing` can detect "this builtin is gone" via filesystem
/// existence checks.
fn builtins() -> [(&'static str, Factory); 6] {
    [
        ("default_round.oxiebrush", BrushPreset::default_round),
        ("ink_pen.oxiebrush", BrushPreset::ink_pen),
        ("pixel.oxiebrush", BrushPreset::pixel),
        ("scatter_dot.oxiebrush", BrushPreset::scatter_dot),
        ("speed_brush.oxiebrush", BrushPreset::speed_brush),
        ("chalk.oxiebrush", BrushPreset::debug_chalk),
    ]
}

/// Absolute paths every builtin archive *would* live at under `dir`.
/// Useful for the repopulate UI to compute the missing set.
pub fn builtin_paths(dir: &Path) -> Vec<PathBuf> {
    builtins().iter().map(|(name, _)| dir.join(name)).collect()
}

/// Write any missing builtin archives to `dir`. Creates `dir` if it
/// doesn't exist. Returns the number of files actually written.
///
/// Also migrates older installs: if a builtin archive exists on disk
/// but is missing its icon while the embedded factory now ships one,
/// the disk copy is overwritten. This keeps the iconless `.oxiebrush`
/// files written by stage-5 installs from sticking around forever.
pub fn seed_missing(dir: &Path) -> Result<usize, BrushError> {
    std::fs::create_dir_all(dir)?;
    let mut written = 0;
    for (name, factory) in builtins() {
        let path = dir.join(name);
        let needs_write = if path.exists() {
            let embedded_has_icon = factory(PLACEHOLDER_ID).icon.is_some();
            if embedded_has_icon {
                match format::load(&path) {
                    Ok(pkg) => pkg.icon.is_none(),
                    Err(_) => false, // unreadable file -> leave alone
                }
            } else {
                false
            }
        } else {
            true
        };
        if needs_write {
            let preset = factory(PLACEHOLDER_ID);
            format::save(&preset, &path)?;
            written += 1;
        }
    }
    Ok(written)
}

/// Force-write every builtin archive, overwriting existing files.
/// Intended for the "Reset built-in brushes" affordance once the UI
/// lands; not wired from anywhere automatic.
pub fn seed_all(dir: &Path) -> Result<usize, BrushError> {
    std::fs::create_dir_all(dir)?;
    let mut written = 0;
    for (name, factory) in builtins() {
        let path = dir.join(name);
        let preset = factory(PLACEHOLDER_ID);
        format::save(&preset, &path)?;
        written += 1;
    }
    Ok(written)
}

/// The last-resort brush used when no disk brushes can be loaded.
/// Returned so the engine can stay functional in headless / corrupt
/// installs.
pub fn fallback_brush() -> BrushPreset {
    BrushPreset::default_round(PLACEHOLDER_ID)
}
