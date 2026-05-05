use oxiedraw_utils::geometry::Rect;

#[derive(Debug, Default)]
pub struct Compositor {
    dirty: Vec<Rect>,
    full_redraw: bool,
}

impl Compositor {
    pub const fn new() -> Self {
        Self {
            dirty: Vec::new(),
            full_redraw: true,
        }
    }

    pub fn mark_dirty(&mut self, rect: Rect) {
        self.dirty.push(rect);
    }

    pub fn request_full_redraw(&mut self) {
        self.full_redraw = true;
        self.dirty.clear();
    }

    #[must_use]
    pub fn take_dirty(&mut self) -> (bool, Vec<Rect>) {
        let full = std::mem::replace(&mut self.full_redraw, false);
        let rects = std::mem::take(&mut self.dirty);
        (full, rects)
    }
}
