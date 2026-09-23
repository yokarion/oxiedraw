//! Cairo paths shared by the drawn widgets.

use relm4::gtk::cairo;

/// Trace a rounded rectangle as the current path. The radius is clamped to
/// what the box can hold, so a squat rectangle rounds to a stadium.
pub(crate) fn rounded_rect(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    radius: f64,
) {
    let radius = radius.max(0.0).min(width / 2.0).min(height / 2.0);
    let turn = std::f64::consts::TAU;
    cr.new_path();
    cr.arc(x + radius, y + radius, radius, turn / 2.0, turn * 0.75);
    cr.arc(x + width - radius, y + radius, radius, turn * 0.75, turn);
    cr.arc(x + width - radius, y + height - radius, radius, 0.0, turn / 4.0);
    cr.arc(x + radius, y + height - radius, radius, turn / 4.0, turn / 2.0);
    cr.close_path();
}
