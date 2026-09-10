//! The dockable panels, one module each. Each is a plain widget built by a
//! `build(...)` function and knows nothing about where it ends up, which is
//! what lets the dock put it anywhere.

pub(crate) mod canvas_info;
pub(crate) mod color_picker;
pub(crate) mod layers;
pub(crate) mod tool_bar;
pub(crate) mod tool_options;
pub(crate) mod tool_windows;
