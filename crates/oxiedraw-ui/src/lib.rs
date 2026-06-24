//! OxieDraw GTK/relm4 UI crate.
//!
//! The only crate that touches GTK/relm4. One `relm4` component (`AppModel`)
//! owns the whole window; panels are plain widgets, each exposing a
//! `pub(crate) fn build() -> <Widget>`. The canvas is the exception: its
//! `gtk::Picture` stays inline in the `view!` tree so `init` can grab the
//! realized widget and call `canvas::wire(...)`.

mod actions;
mod adjustments;
mod app;
mod brush_manager;
mod brush_picker;
mod canvas;
mod canvas_paintable;
mod clipboard;
mod export_window;
mod filters;
mod font_previews;
mod left_bar;
mod perf_graph;
mod preferences_window;
mod project_io;
mod right_bar;
mod session;
mod settings;
mod splash;
mod tabs;
mod text_edit;
mod toaster;
mod tool_options_bar;
mod top_bar;
mod widgets;

use std::process::ExitCode;

use relm4::RelmApp;

const APP_ID: &str = "com.yokarion.oxiedraw";

/// Resource path the icon tree is registered under; see `build.rs`.
const ICON_RESOURCE_PATH: &str = "/io/github/yokarion/OxieDraw/icons";

// hicolor icon tree (tool symbolics + themed app icon), compiled into a
// GResource by `build.rs` so the binary is self-contained.
const ICON_GRESOURCE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/oxiedraw-icons.gresource"));

// Registers the embedded icon GResource. Leaked into a `'static` so the
// registration outlives the process; gio holds the bytes for icon lookups.
fn register_icon_resources() {
    use gtk::{gio, glib};

    let bytes = glib::Bytes::from_static(ICON_GRESOURCE);
    match gio::Resource::from_data(&bytes) {
        Ok(resource) => gio::resources_register(&resource),
        Err(e) => tracing::error!(%e, "failed to register embedded icon resources"),
    }
}

/// Process-start instant, set at the top of [`run`]. Used to log the time to a
/// ready-to-use window.
pub(crate) static STARTUP: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

#[must_use]
pub fn run() -> ExitCode {
    let _ = STARTUP.set(std::time::Instant::now());
    register_icon_resources();
    // The splash drives the slow startup work and reveals the main window when
    // it finishes, so the window must not be auto-shown on activate.
    let app = RelmApp::new(APP_ID).visible_on_activate(false);
    app.run::<app::AppModel>(app::AppInit::default());
    ExitCode::SUCCESS
}
