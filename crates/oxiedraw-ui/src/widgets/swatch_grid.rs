//! A drawn grid of colour swatches: click to pick, drag to reorder, and a
//! trailing "+" cell where one is wanted. Used by the palette dock, the Manage
//! Palettes window and the generator preview, so the geometry and the hit
//! testing live here rather than three times over.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_core::color::Color;
use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::prelude::*;

/// Preferred swatch size. Cells stretch from here to fill the width evenly, so
/// the grid never leaves a ragged column on the right.
const CELL_TARGET: f64 = 24.0;
const GAP: f64 = 4.0;
const RADIUS: f64 = 4.0;
/// Pointer travel before a press turns into a reorder rather than a pick.
const DRAG_SLOP: f64 = 6.0;

/// What the grid shows. Replaced wholesale on every refresh.
#[derive(Default, Clone)]
pub(crate) struct GridContent {
    pub(crate) colors: Vec<Color>,
    /// Slots drawn in total; the ones past `colors` are empty placeholders.
    pub(crate) capacity: usize,
    /// Append a trailing "+" cell.
    pub(crate) add_slot: bool,
    /// Whether swatches may be dragged and removed here.
    pub(crate) editable: bool,
}

#[derive(Default)]
struct GridModel {
    content: GridContent,
    selected: Option<usize>,
}

impl GridModel {
    fn slots(&self) -> usize {
        self.content
            .capacity
            .max(self.content.colors.len() + usize::from(self.content.add_slot))
    }

    fn add_index(&self) -> Option<usize> {
        self.content.add_slot.then_some(self.content.colors.len())
    }
}

/// What the grid does when a cell is used. Leaving a hook out disables that
/// interaction: a read-only preset grid has only `pick`.
#[derive(Default, Clone)]
pub(crate) struct GridHooks {
    pub(crate) pick: Option<Rc<dyn Fn(usize)>>,
    /// Given the "+" cell's rectangle, for anything that pops up beside it.
    pub(crate) add: Option<Rc<dyn Fn(gtk::gdk::Rectangle)>>,
    pub(crate) reorder: Option<Rc<dyn Fn(usize, usize)>>,
    pub(crate) remove: Option<Rc<dyn Fn(usize)>>,
}

#[derive(Clone, Copy, Default)]
struct DragState {
    from: Option<usize>,
    active: bool,
    x: f64,
    y: f64,
}

pub(crate) struct SwatchGrid {
    area: gtk::DrawingArea,
    model: Rc<RefCell<GridModel>>,
    drag: Rc<Cell<DragState>>,
    hooks: Rc<RefCell<GridHooks>>,
}

impl SwatchGrid {
    pub(crate) fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .hexpand(true)
            .valign(gtk::Align::Start)
            .build();
        let model = Rc::new(RefCell::new(GridModel::default()));
        let drag = Rc::new(Cell::new(DragState::default()));
        // Late-bound: the owner usually needs the grid handle to build the
        // callbacks that act on it.
        let hooks = Rc::new(RefCell::new(GridHooks::default()));

        {
            let model = Rc::clone(&model);
            let drag = Rc::clone(&drag);
            area.set_draw_func(move |area, cr, w, _| {
                paint(area, cr, w, &model.borrow(), drag.get());
            });
        }
        {
            let model = Rc::clone(&model);
            let area_resize = area.clone();
            area.connect_resize(move |_, w, _| {
                fit_height(&area_resize, w, &model.borrow());
            });
        }

        install_input(&area, &model, &drag, &hooks);

        Self {
            area,
            model,
            drag,
            hooks,
        }
    }

    pub(crate) fn widget(&self) -> gtk::Widget {
        self.area.clone().upcast()
    }

    pub(crate) fn set_hooks(&self, hooks: GridHooks) {
        *self.hooks.borrow_mut() = hooks;
    }

    pub(crate) fn set_content(&self, content: GridContent) {
        {
            let mut model = self.model.borrow_mut();
            model.content = content;
            if model.selected.is_some_and(|i| i >= model.content.colors.len()) {
                model.selected = None;
            }
        }
        self.drag.set(DragState::default());
        fit_height(&self.area, self.area.width(), &self.model.borrow());
        self.area.queue_draw();
    }

    pub(crate) fn set_selected(&self, selected: Option<usize>) {
        if self.model.borrow().selected == selected {
            return;
        }
        self.model.borrow_mut().selected = selected;
        self.area.queue_draw();
    }

    /// Index of the first swatch holding `color`, which is how the panels mark
    /// the active colour without tracking an index of their own.
    pub(crate) fn index_of(&self, color: Color) -> Option<usize> {
        self.model
            .borrow()
            .content
            .colors
            .iter()
            .position(|c| *c == color)
    }

    pub(crate) fn cancel_drag(&self) {
        self.drag.set(DragState::default());
    }
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Geometry {
    cell: f64,
    cols: usize,
}

impl Geometry {
    fn for_width(width: i32) -> Self {
        let width = f64::from(width.max(1));
        let cols = ((width + GAP) / (CELL_TARGET + GAP)).floor().max(1.0);
        let cell = ((width - GAP * (cols - 1.0)) / cols).max(8.0);
        Self {
            cell,
            cols: cols as usize,
        }
    }

    fn origin(self, index: usize) -> (f64, f64) {
        let row = index / self.cols;
        let col = index % self.cols;
        (
            col as f64 * (self.cell + GAP),
            row as f64 * (self.cell + GAP),
        )
    }

    fn cell_rect(self, index: usize) -> gtk::gdk::Rectangle {
        let (x, y) = self.origin(index);
        let size = self.cell.round() as i32;
        gtk::gdk::Rectangle::new(x.round() as i32, y.round() as i32, size, size)
    }

    fn rows(self, slots: usize) -> usize {
        slots.div_ceil(self.cols.max(1)).max(1)
    }

    fn height(self, slots: usize) -> f64 {
        let rows = self.rows(slots) as f64;
        rows * self.cell + (rows - 1.0) * GAP
    }

    /// Slot under the pointer, or `None` past the end of the grid.
    fn hit(self, x: f64, y: f64, slots: usize) -> Option<usize> {
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let col = (x / (self.cell + GAP)).floor() as usize;
        let row = (y / (self.cell + GAP)).floor() as usize;
        if col >= self.cols {
            return None;
        }
        let index = row * self.cols + col;
        (index < slots).then_some(index)
    }

    /// The gap a dragged swatch would drop into, as the index it would sit
    /// *before*: 0 is ahead of everything, `count` is past the last swatch.
    fn drop_boundary(self, x: f64, y: f64, count: usize) -> usize {
        let col = (x / (self.cell + GAP)).round().max(0.0) as usize;
        let row = (y / (self.cell + GAP)).max(0.0) as usize;
        (row * self.cols + col.min(self.cols)).min(count)
    }
}

fn fit_height(area: &gtk::DrawingArea, width: i32, model: &GridModel) {
    let wanted = Geometry::for_width(width).height(model.slots()).ceil() as i32;
    if area.content_height() != wanted {
        area.set_content_height(wanted);
    }
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

fn paint(
    area: &gtk::DrawingArea,
    cr: &cairo::Context,
    width: i32,
    model: &GridModel,
    drag: DragState,
) {
    let geometry = Geometry::for_width(width);
    let slots = model.slots();
    let fg = area.color();
    let empty = (
        f64::from(fg.red()),
        f64::from(fg.green()),
        f64::from(fg.blue()),
    );
    let accent = crate::theme::accent(area);

    let dragged = drag.active.then_some(drag.from).flatten();
    let insert =
        dragged.map(|_| geometry.drop_boundary(drag.x, drag.y, model.content.colors.len()));

    for index in 0..slots {
        let (x, y) = geometry.origin(index);
        match model.content.colors.get(index) {
            // The gap the swatch came out of, so the row reads as open.
            Some(_) if dragged == Some(index) => {
                rounded(cr, x, y, geometry.cell);
                cr.set_source_rgba(empty.0, empty.1, empty.2, 0.05);
                cr.fill().ok();
            }
            Some(color) => draw_swatch(cr, x, y, geometry.cell, *color),
            None => draw_empty(cr, x, y, geometry.cell, empty, model.add_index() == Some(index)),
        }
        if model.selected == Some(index) && dragged != Some(index) {
            draw_brush_marker(cr, x, y, geometry.cell);
        }
    }

    if let (Some(from), Some(boundary)) = (dragged, insert) {
        draw_insert_marker(cr, geometry, boundary, model.content.colors.len(), accent);
        if let Some(color) = model.content.colors.get(from) {
            let half = geometry.cell / 2.0;
            draw_swatch(cr, drag.x - half, drag.y - half, geometry.cell, *color);
        }
    }
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, size: f64) {
    crate::widgets::shapes::rounded_rect(cr, x, y, size, size, RADIUS);
}

fn draw_swatch(cr: &cairo::Context, x: f64, y: f64, size: f64, color: Color) {
    rounded(cr, x, y, size);
    cr.set_source_rgb(
        f64::from(color.r) / 255.0,
        f64::from(color.g) / 255.0,
        f64::from(color.b) / 255.0,
    );
    cr.fill_preserve().ok();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.35);
    cr.set_line_width(1.0);
    cr.stroke().ok();
}

fn draw_empty(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    size: f64,
    fg: crate::theme::Rgb,
    plus: bool,
) {
    rounded(cr, x, y, size);
    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.07);
    cr.fill().ok();
    if !plus {
        return;
    }
    let arm = (size / 4.0).min(7.0);
    let (cx, cy) = (x + size / 2.0, y + size / 2.0);
    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.55);
    cr.set_line_width(1.6);
    cr.move_to(cx - arm, cy);
    cr.line_to(cx + arm, cy);
    cr.move_to(cx, cy - arm);
    cr.line_to(cx, cy + arm);
    cr.stroke().ok();
}

/// Brush glyph from the Fluent icon's 20-unit box, spanning y 2..18. Drawn
/// inside the swatch, where the grid's edge cannot clip it.
const BRUSH_HEIGHT: f64 = 16.0;
const BRUSH_CENTER: (f64, f64) = (9.2, 10.0);
/// Share of the cell the glyph's height takes.
const BRUSH_SCALE: f64 = 0.62;

fn trace_brush(cr: &cairo::Context) {
    use std::f64::consts::{FRAC_PI_2, PI};
    // Handle and ferrule.
    cr.move_to(5.0, 10.0);
    cr.line_to(15.0, 10.0);
    cr.line_to(15.0, 9.0);
    cr.arc_negative(13.0, 9.0, 2.0, 0.0, -FRAC_PI_2);
    cr.line_to(12.0, 7.0);
    cr.line_to(12.0, 4.0);
    cr.arc_negative(10.0, 4.0, 2.0, 0.0, -PI);
    cr.line_to(8.0, 7.0);
    cr.line_to(7.0, 7.0);
    cr.arc_negative(7.0, 9.0, 2.0, -FRAC_PI_2, -PI);
    cr.close_path();
    // Bristles.
    cr.move_to(5.0, 11.0);
    cr.line_to(15.0, 11.0);
    cr.line_to(15.0, 18.0);
    cr.line_to(4.0, 18.0);
    cr.curve_to(3.7, 18.0, 3.45, 17.55, 3.58, 17.22);
    cr.curve_to(4.4, 15.6, 5.0, 13.6, 5.0, 11.5);
    cr.close_path();
}

fn draw_brush_marker(cr: &cairo::Context, x: f64, y: f64, size: f64) {
    let scale = size * BRUSH_SCALE / BRUSH_HEIGHT;
    cr.save().ok();
    cr.translate(x + size / 2.0, y + size / 2.0);
    cr.scale(scale, scale);
    cr.translate(-BRUSH_CENTER.0, -BRUSH_CENTER.1);
    trace_brush(cr);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.fill_preserve().ok();
    cr.set_source_rgb(0.0, 0.0, 0.0);
    // One device pixel, whatever the glyph's scale.
    cr.set_line_width(1.0 / scale);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.stroke().ok();
    cr.restore().ok();
}

/// The seam the swatch would drop into. Past the last swatch there is no cell
/// to sit before, so it is drawn against the trailing edge of the one before.
fn draw_insert_marker(
    cr: &cairo::Context,
    geometry: Geometry,
    boundary: usize,
    count: usize,
    accent: crate::theme::Rgb,
) {
    let (x, y) = if boundary < count {
        let (x, y) = geometry.origin(boundary);
        // Kept inside the area: the first column's seam sits at a negative x.
        ((x - GAP / 2.0).max(1.0), y)
    } else {
        let (x, y) = geometry.origin(count.saturating_sub(1));
        (x + geometry.cell + GAP / 2.0, y)
    };
    cr.set_source_rgb(accent.0, accent.1, accent.2);
    cr.set_line_width(2.0);
    cr.move_to(x, y);
    cr.line_to(x, y + geometry.cell);
    cr.stroke().ok();
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn install_input(
    area: &gtk::DrawingArea,
    model: &Rc<RefCell<GridModel>>,
    drag: &Rc<Cell<DragState>>,
    hooks: &Rc<RefCell<GridHooks>>,
) {
    let press: Rc<dyn Fn(f64, f64)> = {
        let area = area.clone();
        let model = Rc::clone(model);
        let drag = Rc::clone(drag);
        let hooks = Rc::clone(hooks);
        Rc::new(move |x, y| {
            let geometry = Geometry::for_width(area.width());
            let (slot, add_slot, count) = {
                let model = model.borrow();
                (
                    geometry.hit(x, y, model.slots()),
                    model.add_index(),
                    model.content.colors.len(),
                )
            };
            let Some(index) = slot else { return };
            if add_slot == Some(index) {
                let add = hooks.borrow().add.clone();
                if let Some(add) = add {
                    add(geometry.cell_rect(index));
                }
                return;
            }
            if index >= count {
                return;
            }
            // Pick first: it can refresh the grid (a pick reorders Recent),
            // and a refresh drops any drag in progress. Arming the drag after
            // it is what keeps press-then-drag working.
            let pick = hooks.borrow().pick.clone();
            if let Some(pick) = pick {
                pick(index);
            }
            drag.set(DragState {
                from: Some(index),
                active: false,
                x,
                y,
            });
        })
    };

    let motion: Rc<dyn Fn(f64, f64, f64)> = {
        let area = area.clone();
        let model = Rc::clone(model);
        let drag = Rc::clone(drag);
        let hooks = Rc::clone(hooks);
        Rc::new(move |x, y, travel| {
            let mut state = drag.get();
            let reorderable =
                model.borrow().content.editable && hooks.borrow().reorder.is_some();
            if state.from.is_none() || !reorderable {
                return;
            }
            state.active |= travel > DRAG_SLOP;
            state.x = x;
            state.y = y;
            drag.set(state);
            if state.active {
                area.queue_draw();
            }
        })
    };

    let release: Rc<dyn Fn()> = {
        let area = area.clone();
        let model = Rc::clone(model);
        let drag = Rc::clone(drag);
        let hooks = Rc::clone(hooks);
        Rc::new(move || {
            let state = drag.replace(DragState::default());
            area.queue_draw();
            let (Some(from), true) = (state.from, state.active) else {
                return;
            };
            let count = model.borrow().content.colors.len();
            let geometry = Geometry::for_width(area.width());
            let boundary = geometry.drop_boundary(state.x, state.y, count);
            // The boundary counts the swatch itself, which a reorder lifts out
            // first, so everything past it shifts down one.
            let to = if from < boundary { boundary - 1 } else { boundary };
            let hook = hooks.borrow().reorder.clone();
            if let Some(reorder) = hook
                && from != to
            {
                reorder(from, to);
            }
        })
    };

    let gesture = gtk::GestureDrag::new();
    gesture.set_button(gtk::gdk::BUTTON_PRIMARY);
    {
        let press = Rc::clone(&press);
        gesture.connect_drag_begin(move |_, x, y| press(x, y));
    }
    {
        let motion = Rc::clone(&motion);
        gesture.connect_drag_update(move |gesture, dx, dy| {
            if let Some((sx, sy)) = gesture.start_point() {
                motion(sx + dx, sy + dy, dx.hypot(dy));
            }
        });
    }
    {
        let release = Rc::clone(&release);
        gesture.connect_drag_end(move |gesture, _, _| {
            // A tablet ends this gesture with the pen still on the glass;
            // committing here would drop the swatch wherever it was halfway
            // through. The stylus controller below closes those drags out.
            if gesture
                .device()
                .is_some_and(|device| device.source() == gtk::gdk::InputSource::Pen)
            {
                return;
            }
            release();
        });
    }
    area.add_controller(gesture);

    // GestureDrag drops continuous pen drags on GTK4, so map the pen directly -
    // same shape as the gradient bar.
    let stylus = gtk::GestureStylus::new();
    stylus.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pen_start = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    {
        let press = Rc::clone(&press);
        let pen_start = Rc::clone(&pen_start);
        stylus.connect_down(move |_, x, y| {
            pen_start.set((x, y));
            press(x, y);
        });
    }
    {
        let motion = Rc::clone(&motion);
        let pen_start = Rc::clone(&pen_start);
        stylus.connect_motion(move |_, x, y| {
            let (sx, sy) = pen_start.get();
            motion(x, y, (x - sx).hypot(y - sy));
        });
    }
    {
        let release = Rc::clone(&release);
        stylus.connect_up(move |_, _, _| release());
    }
    area.add_controller(stylus);

    let secondary = gtk::GestureClick::new();
    secondary.set_button(gtk::gdk::BUTTON_SECONDARY);
    {
        let area_hit = area.clone();
        let model = Rc::clone(model);
        let hooks = Rc::clone(hooks);
        secondary.connect_pressed(move |_, _, x, y| {
            let geometry = Geometry::for_width(area_hit.width());
            let hit = {
                let model = model.borrow();
                if !model.content.editable {
                    return;
                }
                geometry
                    .hit(x, y, model.slots())
                    .filter(|index| *index < model.content.colors.len())
            };
            let hook = hooks.borrow().remove.clone();
            if let (Some(index), Some(remove)) = (hit, hook) {
                remove(index);
            }
        });
    }
    area.add_controller(secondary);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_stretch_to_fill_the_width_without_a_ragged_column() {
        let geometry = Geometry::for_width(300);
        let cols = geometry.cols as f64;
        let spanned = geometry.cell * cols + GAP * (cols - 1.0);
        assert!((spanned - 300.0).abs() < 0.001, "row spans the full width");
        assert!(geometry.cell >= CELL_TARGET && geometry.cell < CELL_TARGET + GAP);
    }

    #[test]
    fn a_narrow_panel_still_gets_one_column() {
        assert_eq!(Geometry::for_width(10).cols, 1);
        assert_eq!(Geometry::for_width(0).cols, 1);
    }

    #[test]
    fn height_grows_by_the_row() {
        let geometry = Geometry::for_width(300);
        let one_row = geometry.height(geometry.cols);
        let two_rows = geometry.height(geometry.cols + 1);
        assert!((two_rows - one_row - geometry.cell - GAP).abs() < 0.001);
        assert_eq!(geometry.rows(0), 1, "an empty grid still reserves a row");
    }

    #[test]
    fn hit_testing_lands_on_the_cell_under_the_pointer() {
        let geometry = Geometry::for_width(300);
        assert_eq!(geometry.hit(1.0, 1.0, 18), Some(0));
        let (x, y) = geometry.origin(9);
        assert_eq!(geometry.hit(x + 2.0, y + 2.0, 18), Some(9));
        assert_eq!(geometry.hit(x + 2.0, y + 2.0, 5), None, "past the last slot");
        assert_eq!(geometry.hit(-2.0, 4.0, 18), None);
    }

    #[test]
    fn a_drop_boundary_is_the_gap_the_swatch_falls_into() {
        let geometry = Geometry::for_width(300);
        assert_eq!(geometry.drop_boundary(0.0, 0.0, 4), 0, "ahead of the first");
        assert_eq!(geometry.drop_boundary(2000.0, 2000.0, 4), 4, "past the last");
        assert_eq!(geometry.drop_boundary(50.0, 50.0, 0), 0);

        // Third gap: just left of the cell at index 3.
        let (x, y) = geometry.origin(3);
        assert_eq!(geometry.drop_boundary(x - 1.0, y + 2.0, 9), 3);
    }

    /// The boundary counts the dragged swatch, which is lifted out before the
    /// insert, so a rightward move used to land one slot too far.
    #[test]
    fn a_rightward_drag_lands_where_the_marker_showed() {
        let destination = |from: usize, boundary: usize| {
            if from < boundary { boundary - 1 } else { boundary }
        };
        let mut colors = vec!['A', 'B', 'C', 'D', 'E'];
        let item = colors.remove(0);
        colors.insert(destination(0, 3), item);
        assert_eq!(colors, vec!['B', 'C', 'A', 'D', 'E']);

        assert_eq!(destination(0, 1), 0, "dropping back into its own gap");
        assert_eq!(destination(3, 1), 1, "leftward moves are unchanged");
    }

    #[test]
    fn slots_cover_the_colours_the_placeholders_and_the_add_cell() {
        let model = GridModel {
            content: GridContent {
                colors: vec![Color::BLACK; 3],
                capacity: 18,
                add_slot: false,
                editable: false,
            },
            selected: None,
        };
        assert_eq!(model.slots(), 18, "recent pads out to its capacity");
        assert_eq!(model.add_index(), None);

        let model = GridModel {
            content: GridContent {
                colors: vec![Color::BLACK; 21],
                capacity: 0,
                add_slot: true,
                editable: true,
            },
            selected: None,
        };
        assert_eq!(model.slots(), 22);
        assert_eq!(model.add_index(), Some(21));
    }
}
