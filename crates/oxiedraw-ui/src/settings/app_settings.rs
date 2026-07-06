use std::collections::HashMap;
use std::path::PathBuf;

use oxiedraw_core::export::ExportSettings;
use serde::{Deserialize, Serialize};

pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AppSettings {
    pub(crate) version: String,
    /// Per-action keybind overrides. `Some(accel)` replaces the default; `None` unbinds.
    pub(crate) keybinds: HashMap<String, Option<String>>,
    pub(crate) appearance: AppearanceSettings,
    #[serde(default)]
    pub(crate) shape_correction: ShapeCorrectionSettings,
    #[serde(default)]
    pub(crate) export: ExportSettings,
    #[serde(default)]
    pub(crate) pixel_view: PixelViewSettings,
    #[serde(default)]
    pub(crate) history: HistorySettings,
    #[serde(default)]
    pub(crate) save: SaveSettings,
    /// Name of the brush that should be active on startup. Falls back to
    /// "Ink Pen" -> "Default Round" -> first brush if not found.
    #[serde(default)]
    pub(crate) default_brush_name: Option<String>,
}

/// Project saving: rolling numbered backups and background autosave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SaveSettings {
    /// Keep the last N project versions next to the file as `<name>-1 ... -N`
    /// (`-N` newest), rotated on every manual save.
    #[serde(default = "default_true")]
    pub(crate) backups_enabled: bool,
    /// How many numbered backups to keep (`-1 ... -N`). Default 3.
    #[serde(default = "default_backup_count")]
    pub(crate) backup_count: usize,
    /// Autosave the open documents in the background. Default on.
    #[serde(default = "default_true")]
    pub(crate) autosave_enabled: bool,
    /// Seconds between autosaves. Default 300 (5 minutes).
    #[serde(default = "default_autosave_interval")]
    pub(crate) autosave_interval_secs: u32,
}

fn default_backup_count() -> usize {
    3
}
fn default_autosave_interval() -> u32 {
    300
}

impl Default for SaveSettings {
    fn default() -> Self {
        Self {
            backups_enabled: true,
            backup_count: default_backup_count(),
            autosave_enabled: true,
            autosave_interval_secs: default_autosave_interval(),
        }
    }
}

impl SaveSettings {
    /// Backups to keep on a manual save: 0 when disabled (which skips rotation).
    pub(crate) fn effective_backup_count(&self) -> usize {
        if self.backups_enabled { self.backup_count } else { 0 }
    }
}

/// Undo/redo behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HistorySettings {
    /// Maximum number of actions kept on the undo stack. Default 256.
    #[serde(default = "default_history_capacity")]
    pub(crate) capacity: usize,
}

fn default_history_capacity() -> usize {
    256
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            capacity: default_history_capacity(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PixelViewSettings {
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default = "default_nearest_threshold")]
    pub(crate) nearest_threshold: f32,
    #[serde(default = "default_true")]
    pub(crate) grid_enabled: bool,
    #[serde(default = "default_grid_threshold")]
    pub(crate) grid_threshold: f32,
}

fn default_nearest_threshold() -> f32 {
    1.0
}
fn default_grid_threshold() -> f32 {
    8.0
}

impl Default for PixelViewSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            nearest_threshold: default_nearest_threshold(),
            grid_enabled: true,
            grid_threshold: default_grid_threshold(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ShapeCorrectionSettings {
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default = "default_trigger_delay_ms")]
    pub(crate) trigger_delay_ms: u32,
    #[serde(default = "default_animation_speed_ms")]
    pub(crate) animation_speed_ms: u32,
    #[serde(default = "default_true")]
    pub(crate) correct_line: bool,
    #[serde(default = "default_true")]
    pub(crate) correct_circle: bool,
    #[serde(default = "default_true")]
    pub(crate) correct_rectangle: bool,
}

fn default_true() -> bool {
    true
}
fn default_trigger_delay_ms() -> u32 {
    1000
}
fn default_animation_speed_ms() -> u32 {
    400
}

impl Default for ShapeCorrectionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_delay_ms: default_trigger_delay_ms(),
            animation_speed_ms: default_animation_speed_ms(),
            correct_line: true,
            correct_circle: true,
            correct_rectangle: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AppearanceSettings {
    pub(crate) show_window_decorations: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            show_window_decorations: true,
        }
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            version: APP_VERSION.to_string(),
            keybinds: HashMap::new(),
            appearance: AppearanceSettings::default(),
            shape_correction: ShapeCorrectionSettings::default(),
            export: ExportSettings::default(),
            pixel_view: PixelViewSettings::default(),
            history: HistorySettings::default(),
            save: SaveSettings::default(),
            default_brush_name: Some("Ink Pen".to_string()),
        }
    }
}

pub(crate) fn config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    #[cfg(not(target_os = "windows"))]
    let base = std::env::var("XDG_CONFIG_HOME").map_or_else(|_| {
            std::env::var("HOME").map_or_else(|_| PathBuf::from("."), |h| PathBuf::from(h).join(".config"))
        }, PathBuf::from);

    base.join("oxiedraw")
}

pub(crate) fn config_path() -> PathBuf {
    config_dir().join("settings.json")
}

/// Per-user data directory (`$XDG_DATA_HOME`/`~/.local/share`, `%LOCALAPPDATA%`
/// on Windows). Holds bulkier artifacts like autosave recovery copies.
pub(crate) fn data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    #[cfg(not(target_os = "windows"))]
    let base = std::env::var("XDG_DATA_HOME").map_or_else(
        |_| {
            std::env::var("HOME").map_or_else(
                |_| PathBuf::from("."),
                |h| PathBuf::from(h).join(".local/share"),
            )
        },
        PathBuf::from,
    );

    base.join("oxiedraw")
}

/// Where autosave keeps recovery copies of documents with no file yet.
pub(crate) fn recovery_dir() -> PathBuf {
    data_dir().join("recovery")
}

impl AppSettings {
    pub(crate) fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), err = %e, "failed to parse settings");
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "failed to read settings");
                Self::default()
            }
        }
    }

    pub(crate) fn save(&self) {
        let dir = config_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(err = %e, "failed to create config directory");
            return;
        }
        match serde_json::to_string_pretty(self) {
            Ok(s) => {
                if let Err(e) = std::fs::write(dir.join("settings.json"), s) {
                    tracing::warn!(err = %e, "failed to write settings");
                }
            }
            Err(e) => tracing::warn!(err = %e, "failed to serialize settings"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_settings_defaults() {
        let s = SaveSettings::default();
        assert!(s.backups_enabled);
        assert_eq!(s.backup_count, 3);
        assert!(s.autosave_enabled);
        assert_eq!(s.autosave_interval_secs, 300);
    }

    #[test]
    fn effective_backup_count_reflects_toggle() {
        let mut s = SaveSettings::default();
        assert_eq!(s.effective_backup_count(), 3);
        s.backups_enabled = false;
        assert_eq!(s.effective_backup_count(), 0, "disabled backups skip rotation");
        s.backups_enabled = true;
        s.backup_count = 7;
        assert_eq!(s.effective_backup_count(), 7);
    }

    #[test]
    fn save_settings_survive_a_json_round_trip() {
        let s = SaveSettings {
            backups_enabled: false,
            backup_count: 9,
            autosave_enabled: false,
            autosave_interval_secs: 30,
        };
        let json = serde_json::to_string(&s).expect("test json");
        let back: SaveSettings = serde_json::from_str(&json).expect("test json");
        assert_eq!(back.backups_enabled, s.backups_enabled);
        assert_eq!(back.backup_count, s.backup_count);
        assert_eq!(back.autosave_enabled, s.autosave_enabled);
        assert_eq!(back.autosave_interval_secs, s.autosave_interval_secs);
    }

    // Old settings.json files predate the `save` block; they must load with the
    // backup/autosave defaults rather than failing to parse.
    #[test]
    fn app_settings_without_save_block_uses_defaults() {
        let legacy = r#"{
            "version": "0.0.1",
            "keybinds": {},
            "appearance": { "show_window_decorations": true }
        }"#;
        let parsed: AppSettings = serde_json::from_str(legacy).expect("legacy settings parse");
        assert!(parsed.save.autosave_enabled);
        assert_eq!(parsed.save.backup_count, 3);
        assert_eq!(parsed.save.autosave_interval_secs, 300);
    }

    // Individually missing save fields fall back to their own defaults.
    #[test]
    fn partial_save_block_fills_missing_fields() {
        let partial = r#"{
            "version": "0.0.1",
            "keybinds": {},
            "appearance": { "show_window_decorations": true },
            "save": { "autosave_interval_secs": 600 }
        }"#;
        let parsed: AppSettings = serde_json::from_str(partial).expect("parse");
        assert_eq!(parsed.save.autosave_interval_secs, 600, "explicit value kept");
        assert!(parsed.save.backups_enabled, "missing field -> default");
        assert_eq!(parsed.save.backup_count, 3);
    }

    #[test]
    fn recovery_dir_sits_under_the_data_dir() {
        assert!(recovery_dir().starts_with(data_dir()));
        assert!(recovery_dir().ends_with("recovery"));
    }
}
