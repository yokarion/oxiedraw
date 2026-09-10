use relm4::gtk::cairo;
use relm4::gtk::graphene;

use crate::theme;

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
    cr.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        turn / 4.0,
    );
    cr.arc(
        x + radius,
        y + height - radius,
        radius,
        turn / 4.0,
        turn / 2.0,
    );
    cr.close_path();
}
