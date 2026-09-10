
use std::cell::Cell;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::layout::{Corner, DockSide, FloatAnchor, FloatSize, MIN_FLOAT_SIZE};

const MARGIN: i32 = 12;
const WIDTH: i32 = 300;

#[derive(Clone)]
pub(crate) struct FloatingFrame {
    root: gtk::Box,
    anchor: Rc<Cell<FloatAnchor>>,
    size: Rc<Cell<FloatSize>>,
}

impl FloatingFrame {
    pub(crate) fn new(child: &impl IsA<gtk::Widget>) -> Self {
        let content = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .min_content_height(0)
            .vexpand(true)
            .child(child)
            .build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .visible(false)
            .build();
        root.add_css_class("oxiedraw-float");

        root.append(&content);

        Self {
            root,
            anchor: Rc::new(Cell::new(FloatAnchor::Corner(Corner::TopRight))),
            size: Rc::new(Cell::new(FloatSize::default())),
        }
    }

    pub(crate) fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    pub(crate) fn natural_size(&self) -> (f32, f32) {
        natural_size(&self.root)
    }

    pub(crate) fn set_open(&self, open: bool) {
        self.root.set_visible(open);
    }

    pub(crate) fn follow_visibility(&self, child: &impl IsA<gtk::Widget>) {
        let child = child.as_ref().clone();
        // `get_visible`, not `is_visible`: the child is inside this frame, so
        // a hidden frame would answer no forever and never open.
        self.root.set_visible(child.get_visible());
        let root = self.root.clone();
        child.connect_visible_notify(move |c| root.set_visible(c.get_visible()));
    }

    pub(crate) fn set_placement(&self, anchor: FloatAnchor, size: FloatSize) {
        self.anchor.set(anchor);
        self.size.set(size);
        Self::place(&self.root, anchor, size);
    }

    pub(crate) fn replace(&self) {
        Self::place(&self.root, self.anchor.get(), self.size.get());
    }

    pub(crate) fn anchor(&self) -> FloatAnchor {
        self.anchor.get()
    }

    fn place(root: &gtk::Box, anchor: FloatAnchor, size: FloatSize) {
        let (halign, valign) = match anchor {
            FloatAnchor::Corner(Corner::TopLeft) => (gtk::Align::Start, gtk::Align::Start),
            FloatAnchor::Corner(Corner::TopRight) => (gtk::Align::End, gtk::Align::Start),
            FloatAnchor::Corner(Corner::BottomLeft) => (gtk::Align::Start, gtk::Align::End),
            FloatAnchor::Corner(Corner::BottomRight) => (gtk::Align::End, gtk::Align::End),
            FloatAnchor::Edge(DockSide::Top) => (gtk::Align::Fill, gtk::Align::Start),
            FloatAnchor::Edge(DockSide::Bottom) => (gtk::Align::Fill, gtk::Align::End),
            FloatAnchor::Edge(DockSide::Left) => (gtk::Align::Start, gtk::Align::Fill),
            FloatAnchor::Edge(DockSide::Right) => (gtk::Align::End, gtk::Align::Fill),
        };
        root.set_halign(halign);
        root.set_valign(valign);
        root.set_margin_top(MARGIN);
        root.set_margin_bottom(MARGIN);
        root.set_margin_start(MARGIN);
        root.set_margin_end(MARGIN);

        let room = room_for(root);
        let free = anchor.free_edges();
        let width = fit(halign, (free.0 != 0).then(|| size.width.unwrap_or(WIDTH)), room.0);
        let height = fit(valign, (free.1 != 0).then_some(size.height).flatten(), room.1);

        // Both ends: a size request is a floor and can only grow the window past
        // its contents; the scroller's maximum is what pulls it in past them.
        root.set_size_request(width.unwrap_or(-1), height.unwrap_or(-1));
        if let Some(scroller) = content(root) {
            scroller.set_max_content_width(width.unwrap_or(-1));
            scroller.set_max_content_height(height.unwrap_or(-1));
        }
    }

}

fn fit(align: gtk::Align, want: Option<i32>, room: Option<i32>) -> Option<i32> {
    if align == gtk::Align::Fill {
        return None;
    }
    let want = want?;
    Some(room.map_or(want, |room| want.min(room)))
}

fn room_for(root: &gtk::Box) -> (Option<i32>, Option<i32>) {
    let Some(canvas) = root.parent() else {
        return (None, None);
    };
    let axis = |length: i32| (length > 0).then(|| (length - MARGIN * 2).max(MIN_FLOAT_SIZE));
    (axis(canvas.width()), axis(canvas.height()))
}

fn content(root: &gtk::Box) -> Option<gtk::ScrolledWindow> {
    root.last_child()?.downcast::<gtk::ScrolledWindow>().ok()
}

fn natural_size(root: &gtk::Box) -> (f32, f32) {
    let width = root.measure(gtk::Orientation::Horizontal, -1).1.max(WIDTH);
    let height = root.measure(gtk::Orientation::Vertical, width).1;
    #[allow(clippy::cast_precision_loss)]
    (width as f32, height as f32)
}

pub(crate) fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".oxiedraw-float {
            background-color: @popover_bg_color;
            border-radius: 10px;
            border: 1px solid color-mix(in srgb, currentColor 12%, transparent);
            box-shadow: 0 2px 10px rgba(0, 0, 0, 0.35);
        }

        .oxiedraw-float > scrolledwindow > viewport {
            border-radius: 9px;
        }

        /* Panels moved here from the sidebar carry the chrome background; on a
           floating card it would paint a square behind the rounded corners. */
        .oxiedraw-float .oxiedraw-chrome {
            background-color: transparent;
            border-style: none;
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::fit;
    use relm4::gtk;

    #[test]
    fn a_stored_size_is_a_maximum_not_a_minimum() {
        let free = gtk::Align::Start;
        assert_eq!(fit(free, Some(400), Some(900)), Some(400));
        assert_eq!(fit(free, Some(400), Some(300)), Some(300));
        assert_eq!(fit(free, Some(400), None), Some(400));
    }

    #[test]
    fn a_stretched_axis_takes_its_length_from_the_canvas() {
        assert_eq!(fit(gtk::Align::Fill, Some(400), Some(900)), None);
        assert_eq!(fit(gtk::Align::End, None, Some(900)), None);
    }
}
