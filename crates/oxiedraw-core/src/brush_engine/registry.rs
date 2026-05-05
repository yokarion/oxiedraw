//! Scans on-disk `.oxiebrush` archives and feeds them into the engine.
//!
//! Brushes live under `$XDG_CONFIG_HOME/oxiedraw/brushes` (default
//! `~/.config/oxiedraw/brushes`). On first launch the engine seeds
//! built-in brush files into this directory via `super::builtins`.

use std::path::{Path, PathBuf};

use super::format::{self, BrushError, BrushPackage};

pub struct BrushRegistry;

impl BrushRegistry {
    /// Resolve the brushes directory. Honours `XDG_CONFIG_HOME`, then
    /// falls back to `~/.config/oxiedraw/brushes`. Returns `None` only
    /// when neither env var is set (e.g. embedded contexts).
    pub fn config_dir() -> Option<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            let p = PathBuf::from(xdg);
            if !p.as_os_str().is_empty() {
                return Some(p.join("oxiedraw").join("brushes"));
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("oxiedraw")
                    .join("brushes"),
            );
        }
        None
    }

    /// Walk `dir` and parse every `*.oxiebrush` archive. Returns
    /// `(path, package)` pairs alongside any errors so the caller can
    /// log partial failures without bailing out on the first bad file
    /// AND so the loaded `BrushPreset` can carry its file path through
    /// for later save/delete. Entries are sorted by filename so the
    /// brush list has stable order across reloads.
    pub fn scan_dir(
        dir: &Path,
    ) -> (Vec<(PathBuf, BrushPackage)>, Vec<(PathBuf, BrushError)>) {
        let mut packages = Vec::new();
        let mut errors = Vec::new();
        let read_dir = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => {
                // Missing dir is fine - first-time users won't have one.
                if e.kind() != std::io::ErrorKind::NotFound {
                    errors.push((dir.to_path_buf(), BrushError::Io(e)));
                }
                return (packages, errors);
            }
        };
        let mut paths: Vec<PathBuf> = read_dir
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("oxiebrush"))
            .collect();
        paths.sort();
        for path in paths {
            match format::load(&path) {
                Ok(pkg) => packages.push((path, pkg)),
                Err(e) => errors.push((path, e)),
            }
        }
        (packages, errors)
    }
}
