use relm4::gtk::cairo;
use relm4::gtk::graphene;

use crate::theme;
pub(crate) use crate::widgets::shapes::rounded_rect;

const PAD: f64 = 6.0;
const RADIUS: f64 = 8.0;
const LINE: f64 = 2.0;

pub(crate) fn draw(cr: &cairo::Context, rect: graphene::Rect, accent: theme::Rgb, landing: bool) {
    let pad = PAD
        .min(f64::from(rect.width()) / 4.0)
        .min(f64::from(rect.height()) / 4.0);
    let (x, y, w, h) = (
        f64::from(rect.x()) + pad,
        f64::from(rect.y()) + pad,
        f64::from(rect.width()) - pad * 2.0,
        f64::from(rect.height()) - pad * 2.0,
    );
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let radius = RADIUS.min(w / 2.0).min(h / 2.0);
    let (fill, line) = if landing { (0.22, 0.85) } else { (0.10, 0.35) };

    rounded_rect(cr, x, y, w, h, radius);
    cr.set_source_rgba(accent.0, accent.1, accent.2, fill);
    let _ = cr.fill();

    let half_line = LINE / 2.0;
    rounded_rect(cr, x + half_line, y + half_line, w - LINE, h - LINE, radius);
    cr.set_source_rgba(accent.0, accent.1, accent.2, line);
    cr.set_line_width(LINE);
    let _ = cr.stroke();
}

