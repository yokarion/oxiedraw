// The stylesheet only reaches widgets, so anything drawn with cairo looks its
// colors up here. Fallbacks are libadwaita's own dark-theme values.

use relm4::gtk;
use relm4::gtk::prelude::*;

pub(crate) type Rgb = (f64, f64, f64);

const FALLBACK_WARNING_GROUND: Rgb = (0.804, 0.576, 0.035); // #cd9309
const FALLBACK_WARNING_ACCENT: Rgb = (1.0, 0.761, 0.322); // #ffc252
const FALLBACK_ACCENT: Rgb = (0.208, 0.518, 0.894); // #3584e4
const FALLBACK_DESTRUCTIVE: Rgb = (0.878, 0.106, 0.141); // #e01b24
const FALLBACK_WINDOW_BG: Rgb = (0.133, 0.133, 0.149); // #222226

// The off-canvas backdrop sits a shade below the window so the page edge reads.
const CANVAS_BACKDROP_SHADE: f64 = 0.9;

// Tracks the `button.alpha-lock:checked` rule in the layers panel.
pub(crate) const WARNING_WASH_ALPHA: f64 = 0.26;

pub(crate) fn warning_accent(widget: &impl IsA<gtk::Widget>) -> Rgb {
    lookup(widget, "warning_color").unwrap_or(FALLBACK_WARNING_ACCENT)
}

pub(crate) fn warning_ground(widget: &impl IsA<gtk::Widget>) -> Rgb {
    lookup(widget, "warning_bg_color").unwrap_or(FALLBACK_WARNING_GROUND)
}

pub(crate) fn warning_fg(widget: &impl IsA<gtk::Widget>) -> Rgb {
    lookup(widget, "warning_fg_color").unwrap_or((0.0, 0.0, 0.0))
}

pub(crate) fn accent(widget: &impl IsA<gtk::Widget>) -> Rgb {
    lookup(widget, "accent_color").unwrap_or(FALLBACK_ACCENT)
}

pub(crate) fn destructive(widget: &impl IsA<gtk::Widget>) -> Rgb {
    lookup(widget, "destructive_bg_color")
        .or_else(|| lookup(widget, "error_bg_color"))
        .unwrap_or(FALLBACK_DESTRUCTIVE)
}

pub(crate) fn canvas_backdrop(widget: &impl IsA<gtk::Widget>) -> Rgb {
    let (r, g, b) = lookup(widget, "window_bg_color").unwrap_or(FALLBACK_WINDOW_BG);
    (r * CANVAS_BACKDROP_SHADE, g * CANVAS_BACKDROP_SHADE, b * CANVAS_BACKDROP_SHADE)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn to_rgb8((r, g, b): Rgb) -> [u8; 3] {
    let byte = |v: f64| (v * 255.0).round() as u8;
    [byte(r), byte(g), byte(b)]
}

fn lookup(widget: &impl IsA<gtk::Widget>, name: &str) -> Option<Rgb> {
    #[allow(deprecated)]
    let rgba = widget.as_ref().style_context().lookup_color(name)?;
    Some((
        f64::from(rgba.red()),
        f64::from(rgba.green()),
        f64::from(rgba.blue()),
    ))
}
