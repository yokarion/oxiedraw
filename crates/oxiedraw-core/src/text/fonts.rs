//! Font management for text layers.
//!
//! [`TextEngine`] wraps the cosmic-text font database (system fonts plus any
//! fonts loaded from a project at runtime) and is shared app-wide. The
//! per-document [`FontRegistry`] holds the raw bytes of every font the document
//! embeds, so a saved project renders the same text even on a machine where the
//! font isn't installed.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};

use cosmic_text::{FontSystem, SwashCache, fontdb};
use serde::{Deserialize, Serialize};

/// One parsed font face record, re-exported so callers outside this crate can
/// name the result of [`spawn_font_load`] without depending on `fontdb`.
pub use fontdb::FaceInfo;

/// Content hash of font-file bytes as a 16-hex string. Used to dedup embedded
/// fonts and to name their files inside the project archive.
#[must_use]
pub fn font_hash(bytes: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// One embedded font file plus the family names it provides. The bytes are
/// shared (`Rc`) because the same file is also handed to the font database.
#[derive(Clone)]
pub struct EmbeddedFont {
    pub hash: String,
    pub families: Vec<String>,
    pub bytes: Rc<Vec<u8>>,
}

impl std::fmt::Debug for EmbeddedFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedFont")
            .field("hash", &self.hash)
            .field("families", &self.families)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Serializable metadata for one embedded font. The bytes live in a separate
/// archive file named by `hash`; only this record goes into the JSON manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FontMeta {
    pub hash: String,
    pub families: Vec<String>,
}

/// Per-document set of embedded font files, keyed by content hash.
#[derive(Default, Clone)]
pub struct FontRegistry {
    by_hash: HashMap<String, EmbeddedFont>,
}

impl FontRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Embed a font file, deduplicated by content hash. Re-embedding identical
    /// bytes is a no-op; returns the (existing or new) hash.
    pub fn embed(&mut self, families: Vec<String>, bytes: Vec<u8>) -> String {
        let hash = font_hash(&bytes);
        self.by_hash
            .entry(hash.clone())
            .or_insert_with(|| EmbeddedFont {
                hash: hash.clone(),
                families,
                bytes: Rc::new(bytes),
            });
        hash
    }

    /// Insert a pre-built entry (used by project load).
    pub fn insert(&mut self, font: EmbeddedFont) {
        self.by_hash.insert(font.hash.clone(), font);
    }

    #[must_use]
    pub fn get(&self, hash: &str) -> Option<&EmbeddedFont> {
        self.by_hash.get(hash)
    }

    /// `true` if any embedded font provides this family.
    #[must_use]
    pub fn contains_family(&self, family: &str) -> bool {
        self.by_hash
            .values()
            .any(|f| f.families.iter().any(|fam| fam == family))
    }

    pub fn iter(&self) -> impl Iterator<Item = &EmbeddedFont> {
        self.by_hash.values()
    }

    /// Serializable metadata for every embedded font (for the archive manifest).
    #[must_use]
    pub fn metadata(&self) -> Vec<FontMeta> {
        self.by_hash
            .values()
            .map(|f| FontMeta {
                hash: f.hash.clone(),
                families: f.families.clone(),
            })
            .collect()
    }
}

impl std::fmt::Debug for FontRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontRegistry")
            .field("fonts", &self.by_hash.len())
            .finish()
    }
}

/// Shared text shaping context owning the font database and the glyph cache.
/// One instance is shared app-wide via `Rc<RefCell<TextEngine>>`; rendering
/// borrows it mutably to shape and rasterize glyphs.
pub struct TextEngine {
    pub font_system: FontSystem,
    pub swash_cache: SwashCache,
}

impl TextEngine {
    /// Create an engine with the system fonts loaded.
    #[must_use]
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
        }
    }

    /// Create an engine with an empty font database. The system fonts are
    /// parsed afterwards off-thread via [`spawn_font_load`] (over the list from
    /// [`system_font_files`]) and merged in with [`Self::add_faces`], so startup
    /// can show progress instead of blocking on a full system scan.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            font_system: FontSystem::new_with_locale_and_db(default_locale(), fontdb::Database::new()),
            swash_cache: SwashCache::new(),
        }
    }

    /// Load a single font file into the database. Unreadable files or non-font
    /// data are ignored - one bad file shouldn't abort startup.
    pub fn load_font_path(&mut self, path: &Path) {
        let _ = self.font_system.db_mut().load_font_file(path);
    }

    /// Insert already-parsed font faces into the database. Used to merge the
    /// results of a parallel background parse (see [`spawn_font_load`]) into the
    /// shared engine on the main thread; each face only references its file
    /// path, so this is a cheap set of inserts (no re-reading of the fonts).
    pub fn add_faces(&mut self, faces: Vec<fontdb::FaceInfo>) {
        let db = self.font_system.db_mut();
        for face in faces {
            db.push_face_info(face);
        }
    }

    /// All distinct font family names in the database, sorted.
    #[must_use]
    pub fn available_families(&self) -> Vec<String> {
        let mut set = BTreeSet::new();
        for face in self.font_system.db().faces() {
            for (family, _lang) in &face.families {
                set.insert(family.clone());
            }
        }
        set.into_iter().collect()
    }

    /// A sensible default family name for new text: the first of a few common
    /// sans-serif families that exists, else any available family, else the
    /// generic "sans-serif" (which cosmic-text resolves to a real font).
    #[must_use]
    pub fn default_family(&self) -> String {
        const PREFERRED: [&str; 6] = [
            "Inter",
            "DejaVu Sans",
            "Liberation Sans",
            "Noto Sans",
            "Helvetica",
            "Arial",
        ];
        for name in PREFERRED {
            if self.has_family(name) {
                return name.to_string();
            }
        }
        self.available_families()
            .into_iter()
            .next()
            .unwrap_or_else(|| "sans-serif".to_string())
    }

    /// `true` if a family with this name exists in the database.
    #[must_use]
    pub fn has_family(&self, family: &str) -> bool {
        self.font_system
            .db()
            .faces()
            .any(|face| face.families.iter().any(|(f, _)| f == family))
    }

    /// Build a [`FontRegistry`] embedding the font files that back any of the
    /// given families (for saving into a project). Each face's file is read
    /// from the database (loading from disk for system fonts) and deduped by
    /// content hash.
    #[must_use]
    pub fn embed_used_fonts(&self, families: &HashSet<String>) -> FontRegistry {
        let mut registry = FontRegistry::new();
        let db = self.font_system.db();
        for face in db.faces() {
            let provides: Vec<String> = face.families.iter().map(|(f, _)| f.clone()).collect();
            if provides.iter().any(|f| families.contains(f))
                && let Some(bytes) = db.with_face_data(face.id, |data, _| data.to_vec())
            {
                registry.embed(provides, bytes);
            }
        }
        registry
    }

    /// Load a font file into the database. Returns the family names it added
    /// that were not already present (so the caller can update its UI / embed).
    pub fn load_font_data(&mut self, bytes: Vec<u8>) -> Vec<String> {
        let before = self.family_set();
        self.font_system.db_mut().load_font_data(bytes);
        let after = self.family_set();
        let mut added: Vec<String> = after.difference(&before).cloned().collect();
        added.sort();
        added
    }

    fn family_set(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        for face in self.font_system.db().faces() {
            for (family, _lang) in &face.families {
                set.insert(family.clone());
            }
        }
        set
    }
}

impl Default for TextEngine {
    fn default() -> Self {
        Self::new()
    }
}

// FontSystem/SwashCache aren't Debug; summarize the loaded face count.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for TextEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextEngine")
            .field("faces", &self.font_system.db().faces().count())
            .finish()
    }
}

/// The system locale (e.g. "en-US"), used by cosmic-text for per-language font
/// family name selection. Mirrors cosmic-text's own default in `FontSystem::new`.
fn default_locale() -> String {
    std::env::var("LANG")
        .ok()
        .and_then(|lang| lang.split('.').next().map(|s| s.replace('_', "-")))
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "en-US".to_string())
}

/// Standard directories holding installed fonts, by platform. Mirrors fontdb's
/// own search list so the incremental loader ends up with the same font set.
fn system_font_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "linux")]
    {
        dirs.push("/usr/share/fonts".into());
        dirs.push("/usr/local/share/fonts".into());
        dirs.push("/run/host/usr/share/fonts".into()); // flatpak host fonts
        if let Ok(home) = std::env::var("HOME") {
            let home = PathBuf::from(home);
            dirs.push(home.join(".fonts"));
            dirs.push(home.join(".local/share/fonts"));
        }
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            dirs.push(PathBuf::from(xdg).join("fonts"));
        }
    }

    #[cfg(target_os = "macos")]
    {
        dirs.push("/Library/Fonts".into());
        dirs.push("/System/Library/Fonts".into());
        dirs.push("/Network/Library/Fonts".into());
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Fonts"));
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(root) = std::env::var("SystemRoot") {
            dirs.push(PathBuf::from(root).join("Fonts"));
        } else {
            dirs.push("C:\\Windows\\Fonts".into());
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            let home = PathBuf::from(home);
            dirs.push(home.join("AppData\\Local\\Microsoft\\Windows\\Fonts"));
            dirs.push(home.join("AppData\\Roaming\\Microsoft\\Windows\\Fonts"));
        }
    }

    dirs
}

/// Parse the given font files into face records off the main thread, split
/// across all available CPUs. `parsed` is bumped once per file so the caller can
/// show a running count while the UI stays live; the merged faces arrive on the
/// returned channel when every worker finishes. Feed the result to
/// [`TextEngine::add_faces`].
///
/// Parsing the files is by far the slowest part of startup (a system with a few
/// thousand fonts spends seconds here); doing it serially on the UI thread froze
/// the splash, so it runs in parallel in the background instead.
#[must_use]
pub fn spawn_font_load(files: Vec<PathBuf>, parsed: Arc<AtomicUsize>) -> Receiver<Vec<fontdb::FaceInfo>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(parse_faces_parallel(&files, &parsed));
    });
    rx
}

/// Parse every file's faces across a scoped thread pool. Each worker builds its
/// own throwaway `Database` (they can't share one) and hands back the parsed
/// `FaceInfo`s, which only hold the file path - cheap to move and merge.
fn parse_faces_parallel(files: &[PathBuf], parsed: &AtomicUsize) -> Vec<fontdb::FaceInfo> {
    if files.is_empty() {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len());
    let chunk_size = files.len().div_ceil(workers);

    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    let mut db = fontdb::Database::new();
                    for path in chunk {
                        let _ = db.load_font_file(path);
                        parsed.fetch_add(1, Ordering::Relaxed);
                    }
                    db.faces().cloned().collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// Enumerate candidate font files under the standard system font directories,
/// recursively. Returned as a list so a caller can load them one at a time and
/// report a running count to the user.
#[must_use]
pub fn system_font_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut seen_files = HashSet::new();
    let mut seen_dirs = HashSet::new();
    for dir in system_font_dirs() {
        collect_font_files(&dir, &mut files, &mut seen_files, &mut seen_dirs);
    }
    files
}

fn collect_font_files(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    seen_files: &mut HashSet<PathBuf>,
    seen_dirs: &mut HashSet<PathBuf>,
) {
    // `is_dir()` follows symlinks, so a directory symlinked to an ancestor would
    // recurse forever. Track canonical directory paths and skip any already
    // descended into (this also avoids rescanning a dir reached two ways, e.g.
    // XDG_DATA_HOME/fonts == ~/.local/share/fonts).
    let canon_dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !seen_dirs.insert(canon_dir) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_font_files(&path, out, seen_files, seen_dirs);
        } else if is_font_file(&path) {
            // Canonicalize so a font reachable through two symlinked dirs is
            // only loaded (and counted) once.
            let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if seen_files.insert(key) {
                out.push(path);
            }
        }
    }
}

fn is_font_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("ttf" | "ttc" | "otf" | "otc")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The parallel background parse must yield the same font families as loading
    // the same files serially into an engine. Skips on machines with no fonts.
    #[test]
    fn parallel_load_matches_serial_families() {
        let files = system_font_files();
        if files.is_empty() {
            return;
        }

        let mut serial = TextEngine::empty();
        for path in &files {
            serial.load_font_path(path);
        }

        let parsed = Arc::new(AtomicUsize::new(0));
        let faces = parse_faces_parallel(&files, &parsed);
        assert_eq!(parsed.load(Ordering::Relaxed), files.len());
        let mut parallel = TextEngine::empty();
        parallel.add_faces(faces);

        assert_eq!(parallel.available_families(), serial.available_families());
    }

    #[test]
    fn hash_is_deterministic_and_content_addressed() {
        let a = font_hash(b"hello font bytes");
        let b = font_hash(b"hello font bytes");
        let c = font_hash(b"different bytes");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn embed_dedups_identical_bytes() {
        let mut reg = FontRegistry::new();
        let h1 = reg.embed(vec!["Inter".into()], b"FONTDATA".to_vec());
        let h2 = reg.embed(vec!["Inter".into()], b"FONTDATA".to_vec());
        assert_eq!(h1, h2);
        assert_eq!(reg.len(), 1);
        assert!(reg.contains_family("Inter"));
        assert!(!reg.contains_family("Roboto"));
        assert_eq!(reg.get(&h1).unwrap().bytes.as_slice(), b"FONTDATA");
    }

    #[test]
    fn metadata_roundtrips_families() {
        let mut reg = FontRegistry::new();
        reg.embed(vec!["A".into(), "B".into()], b"x".to_vec());
        let meta = reg.metadata();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].families, vec!["A".to_string(), "B".to_string()]);
    }
}
