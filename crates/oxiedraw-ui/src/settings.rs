//! User preferences persisted as JSON under the platform config dir
//! (`$XDG_CONFIG_HOME/oxiedraw/settings.json` on Linux, `%APPDATA%` on
//! Windows).
//!
//! `keybinds` is a sparse map of action ID to accelerator override: a string
//! is a custom accel, `null` is explicitly unbound, absent means "use the
//! built-in default". `load()` is infallible (falls back to `Default` and
//! logs); `save()` silently skips writes it cannot perform.
//!
//! `ALL_ACTION_GROUPS` in `keybinds.rs` is the source of truth for rebindable
//! actions; `actions::apply_all_accels` pushes the resolved accels into GTK,
//! which then shows them in menus automatically.

mod app_settings;
pub(crate) mod keybinds;

pub(crate) use app_settings::{APP_VERSION, AppSettings, PixelViewSettings, ShapeCorrectionSettings};
// HistorySettings is reachable via AppSettings::history; export when a preferences page needs it.
#[allow(unused_imports)]
pub(crate) use app_settings::HistorySettings;
