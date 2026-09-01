//! Cursors for the Pattern tool, drawn rather than named.
//!
//! The system set has no cursor for "a click here adds a point", so these two
//! are drawn with cairo and cached - motion events ask for one constantly. The
//! cross matches the one the gradient tool draws on the canvas. Hovering a node
//! uses the plain system `move` cursor.

use std::cell::RefCell;
use std::collections::HashMap;

use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;

use crate::pattern_edit::Hover;

/// Side of the cursor image, in pixels, and where its hotspot sits. The hotspot
/// is in texture pixels, so it and the drawn centre must be the same number or
/// the cursor clicks somewhere other than where it looks.
const SIDE: i32 = 32;
const CENTRE: f64 = 16.0;

/// Cross geometry, matching `draw_gradient_cursor_cairo`.
const ARM: f64 = 10.0;
const GAP: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Shape {
    Draw,
    Add,
}

thread_local! {
    static CACHE: RefCell<HashMap<Shape, gdk::Cursor>> = RefCell::new(HashMap::new());
}

/// The cursor for what the pointer is over. `drawing` holds the plain cross for
/// the whole of a stroke: mid-draw the pointer is on the line it is laying down,
/// so answering the hover would flicker between shapes as you work.
pub(crate) fn for_hover(hover: Hover, drawing: bool) -> Option<gdk::Cursor> {
    if !drawing && matches!(hover, Hover::Node(_)) {
        return gdk::Cursor::from_name("move", None);
    }
    let shape = if drawing || matches!(hover, Hover::Nothing) {
        Shape::Draw
    } else {
        Shape::Add
    };
    CACHE.with(|cache| {
        if let Some(cursor) = cache.borrow().get(&shape) {
            return Some(cursor.clone());
        }
        let cursor = build(shape)?;
        cache.borrow_mut().insert(shape, cursor.clone());
        Some(cursor)
    })
}

fn build(shape: Shape) -> Option<gdk::Cursor> {
    use gtk::cairo::{Context, Format, ImageSurface};

    let mut surface = ImageSurface::create(Format::ARgb32, SIDE, SIDE).ok()?;
    {
        let cr = Context::new(&surface).ok()?;
        cr.set_line_cap(gtk::cairo::LineCap::Butt);
        // Dark halo first, white core over it, so the cross reads over both a
        // white canvas and dark artwork.
        for (width, (r, g, b, a)) in [(3.0, (0.0, 0.0, 0.0, 0.55)), (1.0, (1.0, 1.0, 1.0, 0.95))] {
            cr.set_line_width(width);
            cr.set_source_rgba(r, g, b, a);
            path(&cr, shape);
            cr.stroke().ok();
        }
    }

    let stride = surface.stride();
    let data = surface.data().ok()?;
    let bytes = glib::Bytes::from(&data.to_vec());
    drop(data);
    let texture = gdk::MemoryTexture::new(
        SIDE,
        SIDE,
        // Cairo's ARGB32 is premultiplied BGRA in memory on little-endian.
        gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &bytes,
        stride as usize,
    );
    #[allow(clippy::cast_possible_truncation)]
    Some(gdk::Cursor::from_texture(
        &texture,
        CENTRE as i32,
        CENTRE as i32,
        gdk::Cursor::from_name("crosshair", None).as_ref(),
    ))
}

fn path(cr: &gtk::cairo::Context, shape: Shape) {
    // Half-pixel offset so a 1px line lands on a pixel rather than straddling
    // two, which is what turns a crisp crosshair into a grey smear.
    let c = CENTRE + 0.5;
    cr.move_to(c - ARM, c);
    cr.line_to(c - GAP, c);
    cr.move_to(c + GAP, c);
    cr.line_to(c + ARM, c);
    cr.move_to(c, c - ARM);
    cr.line_to(c, c - GAP);
    cr.move_to(c, c + GAP);
    cr.line_to(c, c + ARM);

    if shape == Shape::Add {
        // Clear of the cross's own arm, so the two do not read as one shape.
        let (bx, by) = (c + 9.0, c + 9.0);
        cr.move_to(bx - 3.0, by);
        cr.line_to(bx + 3.0, by);
        cr.move_to(bx, by - 3.0);
        cr.line_to(bx, by + 3.0);
    }
}
