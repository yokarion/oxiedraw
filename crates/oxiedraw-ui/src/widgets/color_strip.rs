//! A one-line ramp of a palette's colours. Weighted by share where the caller
//! has one (the generator preview), otherwise even. Used for the previews in
//! the Generate Palette and Manage Palettes windows and for the thumbnails in
//! the palette list.

use std::cell::RefCell;
use std::rc::Rc;

use oxiedraw_core::color::Color;
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::prelude::*;

pub(crate) struct ColorStrip {
    area: gtk::DrawingArea,
    bands: Rc<RefCell<Vec<(Color, f32)>>>,
}

impl ColorStrip {
    /// A strip that stretches across whatever width it is given.
    pub(crate) fn filling(height: i32) -> Self {
        let strip = Self::build(height);
        strip.area.set_hexpand(true);
        strip
    }

    pub(crate) fn sized(width: i32, height: i32) -> Self {
        let strip = Self::build(height);
        strip.area.set_content_width(width.max(0));
        strip
    }

    // `content-width` rejects negatives outright - a panic, and one GTK
    // turns into an abort when it fires inside an action callback.
    fn build(height: i32) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_height(height.max(0))
            .build();
        let bands: Rc<RefCell<Vec<(Color, f32)>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let bands = Rc::clone(&bands);
            area.set_draw_func(move |area, cr, w, h| paint(area, cr, w, h, &bands.borrow()));
        }
        Self { area, bands }
    }

    pub(crate) fn widget(&self) -> gtk::Widget {
        self.area.clone().upcast()
    }

    pub(crate) fn set_colors(&self, colors: &[Color]) {
        self.set_weighted(&colors.iter().map(|c| (*c, 1.0)).collect::<Vec<_>>());
    }

    pub(crate) fn set_weighted(&self, bands: &[(Color, f32)]) {
        *self.bands.borrow_mut() = bands.to_vec();
        self.area.queue_draw();
    }
}

const RADIUS: f64 = 8.0;

fn rounded(cr: &cairo::Context, w: f64, h: f64) {
    crate::widgets::shapes::rounded_rect(cr, 0.0, 0.0, w, h, RADIUS);
}

fn paint(
    area: &gtk::DrawingArea,
    cr: &cairo::Context,
    width: i32,
    height: i32,
    bands: &[(Color, f32)],
) {
    let (w, h) = (f64::from(width), f64::from(height));
    let fg = area.color();
    let fg = (
        f64::from(fg.red()),
        f64::from(fg.green()),
        f64::from(fg.blue()),
    );

    cr.save().ok();
    rounded(cr, w, h);
    cr.clip();
    paint_bands(cr, w, h, bands, fg);
    cr.restore().ok();

    // Hairline edge, inset half a pixel so it lands on whole pixels.
    cr.save().ok();
    cr.translate(0.5, 0.5);
    rounded(cr, w - 1.0, h - 1.0);
    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.15);
    cr.set_line_width(1.0);
    cr.stroke().ok();
    cr.restore().ok();
}

fn paint_bands(
    cr: &cairo::Context,
    w: f64,
    h: f64,
    bands: &[(Color, f32)],
    fg: (f64, f64, f64),
) {
    let total: f64 = bands.iter().map(|(_, weight)| f64::from(*weight).max(0.0)).sum();
    if bands.is_empty() || total <= 0.0 {
        cr.rectangle(0.0, 0.0, w, h);
        cr.set_source_rgba(fg.0, fg.1, fg.2, 0.07);
        cr.fill().ok();
        return;
    }
    let mut x = 0.0;
    for (index, (color, weight)) in bands.iter().enumerate() {
        // The last band runs to the edge so rounding never leaves a seam.
        let span = if index + 1 == bands.len() {
            w - x
        } else {
            w * f64::from(weight.max(0.0)) / total
        };
        cr.rectangle(x, 0.0, span.max(0.0), h);
        cr.set_source_rgb(
            f64::from(color.r) / 255.0,
            f64::from(color.g) / 255.0,
            f64::from(color.b) / 255.0,
        );
        cr.fill().ok();
        x += span;
    }
}
