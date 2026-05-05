//! OxieDraw binary entry point.
//!
//! Three crates: `oxiedraw-utils` (no workspace deps), `oxiedraw-core` (engine
//! and state, depends on utils), and `oxiedraw-ui` (relm4/libadwaita, the only
//! crate that touches UI libraries). Types that cross the UI/core boundary live
//! in `core`; core never reaches back into the UI.

use std::process::ExitCode;

fn main() -> ExitCode {
    oxiedraw_utils::tracing::init();
    oxiedraw_ui::run()
}
