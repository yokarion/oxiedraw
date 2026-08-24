//! Theme colours resolved for chrome we draw with cairo.
//!
//! The stylesheet only reaches widgets, so hand-drawn parts look these up
//! themselves. Fallbacks are libadwaita's own dark-theme values.

use relm4::gtk;
use relm4::gtk::prelude::*;

pub(crate) type Rgb = (f64, f64, f64);

const FALLBACK_WARNING_GROUND: Rgb = (0.804, 0.576, 0.035); // #cd9309
const FALLBACK_WARNING_ACCENT: Rgb = (1.0, 0.761, 0.322); // #ffc252

/// Wash opacity behind a lit indicator. Tracks the `button.alpha-lock:checked`
/// rule in the layers panel; change both together.
pub(crate) const WARNING_WASH_ALPHA: f64 = 0.26;

/// The warning pair, used to mark a mode that is on and easy to forget - the
/// same one GNOME lights its microphone and screen-share indicators with. Not
/// the accent: this is a caution, and it has to stay legible on an accent row.
pub(crate) fn warning_accent(widget: &impl IsA<gtk::Widget>) -> Rgb {
    lookup(widget, "warning_color").unwrap_or(FALLBACK_WARNING_ACCENT)
}

pub(crate) fn warning_ground(widget: &impl IsA<gtk::Widget>) -> Rgb {
    lookup(widget, "warning_bg_color").unwrap_or(FALLBACK_WARNING_GROUND)
}

/// What goes on a solid warning fill, for glyphs cut out of the yellow.
pub(crate) fn warning_fg(widget: &impl IsA<gtk::Widget>) -> Rgb {
    lookup(widget, "warning_fg_color").unwrap_or((0.0, 0.0, 0.0))
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
