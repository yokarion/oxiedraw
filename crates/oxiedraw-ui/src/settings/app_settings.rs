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
    /// Name of the brush that should be active on startup. Falls back to
    /// "Ink Pen" -> "Default Round" -> first brush if not found.
    #[serde(default)]
    pub(crate) default_brush_name: Option<String>,
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
