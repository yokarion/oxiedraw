//! Shared, reusable widget builders. Each submodule exposes a
//! `pub(crate) fn build(...) -> <Widget>` shared across panels.

pub(crate) mod boxed_list;
pub(crate) mod canvas_info_bar;
pub(crate) mod gradient_bar;
pub(crate) mod gradient_slider;
pub(crate) mod slider;
pub(crate) mod tool_chip;
