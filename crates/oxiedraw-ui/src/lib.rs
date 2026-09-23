//! The only crate that touches GTK. One relm4 component owns the window, the
//! panels are plain widgets in [`panels`], and where they may go plus the
//! saved arrangements are in [`layout`].

mod actions;
mod adjustments;
mod app;
mod brush_manager;
mod brush_picker;
mod canvas;
mod canvas_paintable;
mod clipboard;
mod dock;
mod export_window;
mod filters;
mod font_previews;
mod layout;
mod pacing_trace;
mod palette_manager;
mod panels;
mod perf_graph;
mod preferences_window;
mod project_io;
mod recording;
mod session;
mod settings;
mod splash;
mod tabs;
mod pattern_cursor;
mod pattern_edit;
mod text_edit;
mod theme;
mod toaster;
mod top_bar;
mod widgets;

use std::process::ExitCode;

use relm4::RelmApp;

const APP_ID: &str = "com.yokarion.oxiedraw";

const ICON_RESOURCE_PATH: &str = "/com/yokarion/oxiedraw/icons";

const ICON_GRESOURCE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/oxiedraw-icons.gresource"));

fn register_icon_resources() {
    use gtk::{gio, glib};

    let bytes = glib::Bytes::from_static(ICON_GRESOURCE);
    match gio::Resource::from_data(&bytes) {
        Ok(resource) => gio::resources_register(&resource),
        Err(e) => tracing::error!(%e, "failed to register embedded icon resources"),
    }
}

pub(crate) static STARTUP: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

#[must_use]
pub fn run() -> ExitCode {
    let _ = STARTUP.set(std::time::Instant::now());
    register_icon_resources();
    // The splash reveals the main window when it finishes, so it must not be
    // auto-shown on activate.
    let app = RelmApp::new(APP_ID).visible_on_activate(false);
    app.run::<app::AppModel>(app::AppInit::default());
    ExitCode::SUCCESS
}
