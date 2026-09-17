//! Photoshop-style tone-curve editor with optional histogram, overlay curves and
//! input/output gradient bars whose handles drag the end points.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_core::curves::{Curve, CurvePoint, LUT_SIZE};
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::gtk::{cairo, gdk, glib};

pub(crate) type Rgb = (f64, f64, f64);

const PAD: f64 = 6.0;
const BAR: f64 = 12.0;
const BAR_GAP: f64 = 6.0;
const HANDLE: f64 = 9.0;
const POINT_HALF: f64 = 3.5;
const HIT_RADIUS: f64 = 9.0;
const DELETE_DISTANCE: f64 = 24.0;
const GRID_DIVISIONS: u32 = 4;
const HEIGHT: i32 = 320;
const SHIFT_STEP: u8 = 10;

const BACKGROUND: Rgb = (0.22, 0.22, 0.22);
const HISTOGRAM: Rgb = (0.45, 0.45, 0.45);

#[derive(Clone, Copy)]
pub(crate) struct Ramp {
    pub low: Rgb,
    pub high: Rgb,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Point(usize),
    /// `true` for the first point, `false` for the last.
    InputHandle(bool),
    OutputHandle(bool),
}

#[derive(Clone, Copy)]
struct Grab {
    target: Target,
    offset: (f64, f64),
    removing: bool,
}

struct State {
    curve: Curve,
    selected: Option<usize>,
    hover: Option<Target>,
    grab: Option<Grab>,
    color: Rgb,
    histogram: Option<Vec<f64>>,
    overlays: Vec<(Curve, Rgb)>,
    input_ramp: Option<Ramp>,
    output_ramp: Option<Ramp>,
}

impl State {
    fn visible_curve(&self) -> Curve {
        let mut curve = self.curve;
        if let Some(Grab {
            target: Target::Point(i),
            removing: true,
            ..
        }) = self.grab
        {
            curve.remove(i);
        }
        curve
    }

    fn layout(&self, area: &gtk::DrawingArea) -> Layout {
        Layout::new(
            f64::from(area.width()),
            f64::from(area.height()),
            self.input_ramp.is_some(),
            self.output_ramp.is_some(),
        )
    }

    fn endpoint(&self, first: bool) -> usize {
        if first { 0 } else { self.curve.points().len() - 1 }
    }
}

#[derive(Clone, Copy)]
struct Layout {
    x: f64,
    y: f64,
    side: f64,
}

impl Layout {
    fn new(width: f64, height: f64, input_bar: bool, output_bar: bool) -> Self {
        let bar_room = BAR_GAP + BAR + HANDLE;
        let left = PAD + if output_bar { bar_room } else { 0.0 };
        let bottom = PAD + if input_bar { bar_room } else { 0.0 };
        let side = (width - left - PAD).min(height - PAD - bottom).max(1.0);
        let spare = (width - left - PAD - side).max(0.0);
        Self {
            x: left + spare / 2.0,
            y: PAD,
            side,
        }
    }

    fn position(&self, point: CurvePoint) -> (f64, f64) {
        (
            self.x + f64::from(point.x) / 255.0 * self.side,
            self.y + (1.0 - f64::from(point.y) / 255.0) * self.side,
        )
    }

    fn level_at(&self, x: f64, y: f64) -> CurvePoint {
        let level = |t: f64| (t.clamp(0.0, 1.0) * 255.0).round() as u8;
        CurvePoint::new(
            level((x - self.x) / self.side),
            level(1.0 - (y - self.y) / self.side),
        )
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        (self.x..=self.x + self.side).contains(&x) && (self.y..=self.y + self.side).contains(&y)
    }

    fn distance_outside(&self, x: f64, y: f64) -> f64 {
        let dx = (self.x - x).max(x - (self.x + self.side)).max(0.0);
        let dy = (self.y - y).max(y - (self.y + self.side)).max(0.0);
        dx.max(dy)
    }

    fn input_bar_top(&self) -> f64 {
        self.y + self.side + BAR_GAP
    }

    fn output_bar_left(&self) -> f64 {
        self.x - BAR_GAP - BAR
    }

    fn input_handle(&self, x: u8) -> (f64, f64) {
        (
            self.x + f64::from(x) / 255.0 * self.side,
            self.input_bar_top() + BAR + HANDLE / 2.0,
        )
    }

    fn output_handle(&self, y: u8) -> (f64, f64) {
        (
            self.output_bar_left() - HANDLE / 2.0,
            self.y + (1.0 - f64::from(y) / 255.0) * self.side,
        )
    }
}

type Handlers = Rc<RefCell<Vec<Rc<dyn Fn(Curve)>>>>;

#[derive(Clone)]
pub(crate) struct CurveEditor {
    pub widget: gtk::DrawingArea,
    state: Rc<RefCell<State>>,
    handlers: Handlers,
}

impl CurveEditor {
    pub(crate) fn new() -> Self {
        let widget = gtk::DrawingArea::builder()
            .height_request(HEIGHT)
            .hexpand(true)
            .focusable(true)
            .build();
        let state = Rc::new(RefCell::new(State {
            curve: Curve::identity(),
            selected: None,
            hover: None,
            grab: None,
            color: (0.92, 0.92, 0.92),
            histogram: None,
            overlays: Vec::new(),
            input_ramp: None,
            output_ramp: None,
        }));
        {
            let state = Rc::clone(&state);
            widget.set_draw_func(move |area, cr, _, _| {
                let st = state.borrow();
                paint(cr, &st, &st.layout(area));
            });
        }
        let handlers: Handlers = Rc::new(RefCell::new(Vec::new()));
        install_input(&widget, &Input {
            area: widget.downgrade(),
            state: Rc::clone(&state),
            handlers: Rc::clone(&handlers),
        });
        Self {
            widget,
            state,
            handlers,
        }
    }

    /// Show `curve` without reporting it as a change.
    pub(crate) fn set_curve(&self, curve: Curve) {
        {
            let mut st = self.state.borrow_mut();
            st.curve = curve;
            st.selected = None;
            st.hover = None;
            st.grab = None;
        }
        self.widget.queue_draw();
    }

    pub(crate) fn set_color(&self, color: Rgb) {
        self.state.borrow_mut().color = color;
        self.widget.queue_draw();
    }

    pub(crate) fn set_histogram(&self, bins: Option<&[u32]>) {
        self.state.borrow_mut().histogram = bins.map(histogram_heights);
        self.widget.queue_draw();
    }

    pub(crate) fn set_overlays(&self, overlays: Vec<(Curve, Rgb)>) {
        self.state.borrow_mut().overlays = overlays;
        self.widget.queue_draw();
    }

    pub(crate) fn set_ramps(&self, input: Option<Ramp>, output: Option<Ramp>) {
        {
            let mut st = self.state.borrow_mut();
            st.input_ramp = input;
            st.output_ramp = output;
        }
        self.widget.queue_draw();
    }

    pub(crate) fn connect_changed(&self, f: impl Fn(Curve) + 'static) {
        self.handlers.borrow_mut().push(Rc::new(f));
    }
}

/// Scaled to the third-largest bin, so paper or line-art spikes don't flatten the rest.
fn histogram_heights(bins: &[u32]) -> Vec<f64> {
    let mut sorted = bins.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    let full = f64::from(sorted.get(2).copied().unwrap_or(0).max(1));
    bins.iter().map(|&b| (f64::from(b) / full).min(1.0)).collect()
}

fn hit_test(st: &State, l: &Layout, x: f64, y: f64) -> Option<Target> {
    let points = st.curve.points();
    let nearest = |candidates: &mut dyn Iterator<Item = (Target, f64)>| {
        candidates
            .filter(|&(_, d)| d <= HIT_RADIUS)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(target, _)| target)
    };
    let point = nearest(&mut points.iter().enumerate().map(|(i, p)| {
        let (px, py) = l.position(*p);
        (Target::Point(i), (px - x).hypot(py - y))
    }));
    if point.is_some() {
        return point;
    }
    let ends = [(true, points[0]), (false, points[points.len() - 1])];
    if st.input_ramp.is_some() {
        let handle = nearest(&mut ends.iter().filter_map(|&(first, p)| {
            let (hx, hy) = l.input_handle(p.x);
            ((y - hy).abs() <= HANDLE).then_some((Target::InputHandle(first), (x - hx).abs()))
        }));
        if handle.is_some() {
            return handle;
        }
    }
    if st.output_ramp.is_some() {
        return nearest(&mut ends.iter().filter_map(|&(first, p)| {
            let (hx, hy) = l.output_handle(p.y);
            ((x - hx).abs() <= HANDLE).then_some((Target::OutputHandle(first), (y - hy).abs()))
        }));
    }
    None
}

#[derive(Clone)]
struct Input {
    // Weak: the controllers holding this belong to the area.
    area: glib::WeakRef<gtk::DrawingArea>,
    state: Rc<RefCell<State>>,
    handlers: Handlers,
}

impl Input {
    /// Handlers run after the borrow ends, so they may call back into the editor.
    fn edit(&self, f: impl FnOnce(&mut State, &Layout)) {
        let Some(area) = self.area.upgrade() else {
            return;
        };
        let changed = {
            let mut st = self.state.borrow_mut();
            let layout = st.layout(&area);
            let before = st.visible_curve();
            f(&mut st, &layout);
            let after = st.visible_curve();
            (after != before).then_some(after)
        };
        area.queue_draw();
        if let Some(curve) = changed {
            let handlers: Vec<_> = self.handlers.borrow().clone();
            for handler in handlers {
                handler(curve);
            }
        }
    }

    fn press(&self, x: f64, y: f64) {
        let Some(area) = self.area.upgrade() else {
            return;
        };
        area.grab_focus();
        self.edit(|st, l| {
            let target = hit_test(st, l, x, y).or_else(|| {
                if !l.contains(x, y) {
                    return None;
                }
                st.curve.insert(l.level_at(x, y)).map(Target::Point)
            });
            let points = st.curve.points();
            let anchor = target.map(|target| match target {
                Target::Point(i) => l.position(points[i]),
                Target::InputHandle(first) => l.input_handle(points[st.endpoint(first)].x),
                Target::OutputHandle(first) => l.output_handle(points[st.endpoint(first)].y),
            });
            st.selected = target.map(|target| match target {
                Target::Point(i) => i,
                Target::InputHandle(first) | Target::OutputHandle(first) => st.endpoint(first),
            });
            st.grab = target.zip(anchor).map(|(target, (ax, ay))| Grab {
                target,
                offset: (ax - x, ay - y),
                removing: false,
            });
            st.hover = target;
        });
        if self.state.borrow().grab.is_some() {
            area.set_cursor_from_name(Some("grabbing"));
        }
    }

    fn drag(&self, x: f64, y: f64) {
        self.edit(|st, l| {
            let Some(mut grab) = st.grab else {
                return;
            };
            let level = l.level_at(x + grab.offset.0, y + grab.offset.1);
            match grab.target {
                Target::Point(i) => {
                    grab.removing = st.curve.points().len() > 2
                        && l.distance_outside(x, y) > DELETE_DISTANCE;
                    if !grab.removing {
                        st.curve.move_point(i, level);
                    }
                }
                Target::InputHandle(first) => {
                    let i = st.endpoint(first);
                    let keep_y = st.curve.points()[i].y;
                    st.curve.move_point(i, CurvePoint::new(level.x, keep_y));
                }
                Target::OutputHandle(first) => {
                    let i = st.endpoint(first);
                    let keep_x = st.curve.points()[i].x;
                    st.curve.move_point(i, CurvePoint::new(keep_x, level.y));
                }
            }
            st.grab = Some(grab);
        });
    }

    fn release(&self, x: f64, y: f64) {
        self.edit(|st, _| {
            if let Some(Grab {
                target: Target::Point(i),
                removing: true,
                ..
            }) = st.grab
            {
                st.curve.remove(i);
                st.selected = None;
            }
            st.grab = None;
        });
        self.hover(x, y);
    }

    fn remove_at(&self, x: f64, y: f64) {
        self.edit(|st, l| {
            if st.grab.is_some() {
                return;
            }
            if let Some(Target::Point(i)) = hit_test(st, l, x, y)
                && st.curve.remove(i)
            {
                st.selected = None;
            }
        });
        self.hover(x, y);
    }

    fn hover(&self, x: f64, y: f64) {
        let Some(area) = self.area.upgrade() else {
            return;
        };
        let (target, in_graph, changed) = {
            let mut st = self.state.borrow_mut();
            if st.grab.is_some() {
                return;
            }
            let layout = st.layout(&area);
            let target = hit_test(&st, &layout, x, y);
            let changed = st.hover != target;
            st.hover = target;
            (target, layout.contains(x, y), changed)
        };
        let cursor = if target.is_some() {
            Some("grab")
        } else if in_graph {
            Some("crosshair")
        } else {
            None
        };
        area.set_cursor_from_name(cursor);
        if changed {
            area.queue_draw();
        }
    }

    fn leave(&self) {
        let Some(area) = self.area.upgrade() else {
            return;
        };
        {
            let mut st = self.state.borrow_mut();
            if st.grab.is_some() {
                return;
            }
            st.hover = None;
        }
        area.set_cursor_from_name(None);
        area.queue_draw();
    }

    fn key(&self, key: gdk::Key, shift: bool) -> bool {
        let step = if shift { SHIFT_STEP } else { 1 };
        let mut handled = false;
        self.edit(|st, _| {
            let count = st.curve.points().len();
            let Some(i) = st.selected.filter(|&i| i < count && st.grab.is_none()) else {
                return;
            };
            let p = st.curve.points()[i];
            let moved = match key {
                gdk::Key::Delete | gdk::Key::KP_Delete | gdk::Key::BackSpace => {
                    if st.curve.remove(i) {
                        st.selected = None;
                    }
                    handled = true;
                    return;
                }
                gdk::Key::Left | gdk::Key::KP_Left => {
                    CurvePoint::new(p.x.saturating_sub(step), p.y)
                }
                gdk::Key::Right | gdk::Key::KP_Right => {
                    CurvePoint::new(p.x.saturating_add(step), p.y)
                }
                gdk::Key::Up | gdk::Key::KP_Up => CurvePoint::new(p.x, p.y.saturating_add(step)),
                gdk::Key::Down | gdk::Key::KP_Down => {
                    CurvePoint::new(p.x, p.y.saturating_sub(step))
                }
                _ => return,
            };
            st.curve.move_point(i, moved);
            handled = true;
        });
        handled
    }
}

fn event_is_stylus(controller: &impl IsA<gtk::EventController>) -> bool {
    controller
        .current_event_device()
        .is_some_and(|d| d.source() == gdk::InputSource::Pen)
}

fn install_input(area: &gtk::DrawingArea, input: &Input) {

    let drag = gtk::GestureDrag::new();
    drag.set_button(gdk::BUTTON_PRIMARY);
    {
        let input = input.clone();
        drag.connect_drag_begin(move |g, x, y| {
            if !event_is_stylus(g) {
                input.press(x, y);
            }
        });
    }
    {
        let input = input.clone();
        drag.connect_drag_update(move |g, dx, dy| {
            if let Some((sx, sy)) = g.start_point().filter(|_| !event_is_stylus(g)) {
                input.drag(sx + dx, sy + dy);
            }
        });
    }
    {
        let input = input.clone();
        drag.connect_drag_end(move |g, dx, dy| {
            if let Some((sx, sy)) = g.start_point().filter(|_| !event_is_stylus(g)) {
                input.release(sx + dx, sy + dy);
            }
        });
    }
    area.add_controller(drag);

    let secondary = gtk::GestureClick::new();
    secondary.set_button(gdk::BUTTON_SECONDARY);
    {
        let input = input.clone();
        secondary.connect_pressed(move |_, _, x, y| input.remove_at(x, y));
    }
    area.add_controller(secondary);

    // GestureDrag drops continuous pen drags on GTK4, so the pen gets its own.
    let stylus = gtk::GestureStylus::new();
    stylus.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pen_down = Rc::new(Cell::new(false));
    {
        let input = input.clone();
        let pen_down = Rc::clone(&pen_down);
        stylus.connect_down(move |_, x, y| {
            pen_down.set(true);
            input.press(x, y);
        });
    }
    {
        let input = input.clone();
        let pen_down = Rc::clone(&pen_down);
        stylus.connect_motion(move |_, x, y| {
            if pen_down.get() {
                input.drag(x, y);
            } else {
                input.hover(x, y);
            }
        });
    }
    {
        let input = input.clone();
        stylus.connect_up(move |_, x, y| {
            pen_down.set(false);
            input.release(x, y);
        });
    }
    area.add_controller(stylus);

    let motion = gtk::EventControllerMotion::new();
    {
        let input = input.clone();
        motion.connect_motion(move |_, x, y| input.hover(x, y));
    }
    {
        let input = input.clone();
        motion.connect_leave(move |_| input.leave());
    }
    area.add_controller(motion);

    let keys = gtk::EventControllerKey::new();
    {
        let input = input.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if input.key(key, modifiers.contains(gdk::ModifierType::SHIFT_MASK)) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    area.add_controller(keys);
}

fn paint(cr: &cairo::Context, st: &State, l: &Layout) {
    cr.rectangle(l.x, l.y, l.side, l.side);
    set_rgb(cr, BACKGROUND);
    let _ = cr.fill();

    if let Some(heights) = &st.histogram {
        paint_histogram(cr, l, heights);
    }
    paint_grid(cr, l);

    let curve = st.visible_curve();
    let _ = cr.save();
    cr.rectangle(l.x, l.y, l.side, l.side);
    cr.clip();
    for (overlay, color) in &st.overlays {
        paint_curve(cr, l, overlay, *color, 0.7, 1.0);
    }
    paint_curve(cr, l, &curve, st.color, 1.0, 1.75);
    let _ = cr.restore();

    // A point being dragged away shifts the indices, so highlight nothing.
    let removing = matches!(st.grab, Some(Grab { removing: true, .. }));
    let grabbed = match st.grab {
        Some(Grab {
            target: Target::Point(i),
            ..
        }) => Some(i),
        _ => None,
    };
    for (i, point) in curve.points().iter().enumerate() {
        let filled = !removing && (st.selected == Some(i) || grabbed == Some(i));
        let hovered = !removing && st.hover == Some(Target::Point(i));
        paint_point(cr, l.position(*point), st.color, filled, hovered);
    }

    let points = curve.points();
    let (first, last) = (points[0], points[points.len() - 1]);
    if let Some(ramp) = st.input_ramp {
        paint_bar(cr, (l.x, l.input_bar_top(), l.side, BAR), ramp, true);
        paint_handle(cr, l.input_handle(first.x), ramp.low, true);
        paint_handle(cr, l.input_handle(last.x), ramp.high, true);
    }
    if let Some(ramp) = st.output_ramp {
        paint_bar(cr, (l.output_bar_left(), l.y, BAR, l.side), ramp, false);
        paint_handle(cr, l.output_handle(first.y), ramp.low, false);
        paint_handle(cr, l.output_handle(last.y), ramp.high, false);
    }
}

fn paint_histogram(cr: &cairo::Context, l: &Layout, heights: &[f64]) {
    let bottom = l.y + l.side;
    let bin_width = l.side / LUT_SIZE as f64;
    cr.move_to(l.x, bottom);
    for (i, h) in heights.iter().enumerate() {
        let top = bottom - h * l.side;
        cr.line_to(l.x + i as f64 * bin_width, top);
        cr.line_to(l.x + (i + 1) as f64 * bin_width, top);
    }
    cr.line_to(l.x + l.side, bottom);
    cr.close_path();
    set_rgb(cr, HISTOGRAM);
    let _ = cr.fill();
}

fn paint_grid(cr: &cairo::Context, l: &Layout) {
    cr.set_line_width(1.0);
    for i in 1..GRID_DIVISIONS {
        let t = (l.side * f64::from(i) / f64::from(GRID_DIVISIONS)).round() + 0.5;
        cr.move_to(l.x + t, l.y);
        cr.line_to(l.x + t, l.y + l.side);
        cr.move_to(l.x, l.y + t);
        cr.line_to(l.x + l.side, l.y + t);
    }
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.12);
    let _ = cr.stroke();

    cr.move_to(l.x, l.y + l.side);
    cr.line_to(l.x + l.side, l.y);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
    let _ = cr.stroke();

    cr.rectangle(l.x + 0.5, l.y + 0.5, l.side - 1.0, l.side - 1.0);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.5);
    let _ = cr.stroke();
}

fn paint_curve(cr: &cairo::Context, l: &Layout, curve: &Curve, color: Rgb, alpha: f64, width: f64) {
    let steps = (l.side.ceil() as usize).max(2);
    let mut samples = vec![0.0_f32; steps];
    curve.sample_into(&mut samples);
    let last = (steps - 1) as f64;
    for (i, v) in samples.iter().enumerate() {
        let x = l.x + i as f64 / last * l.side;
        let y = l.y + (1.0 - f64::from(*v)) * l.side;
        if i == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
    cr.set_source_rgba(color.0, color.1, color.2, alpha);
    cr.set_line_width(width);
    cr.set_line_join(cairo::LineJoin::Round);
    let _ = cr.stroke();
}

fn paint_point(cr: &cairo::Context, (x, y): (f64, f64), color: Rgb, filled: bool, hovered: bool) {
    let half = if hovered { POINT_HALF + 1.0 } else { POINT_HALF };
    cr.rectangle(x - half, y - half, half * 2.0, half * 2.0);
    set_rgb(cr, if filled { color } else { BACKGROUND });
    let _ = cr.fill_preserve();
    set_rgb(cr, color);
    cr.set_line_width(1.25);
    let _ = cr.stroke();
}

fn paint_bar(cr: &cairo::Context, (x, y, w, h): (f64, f64, f64, f64), ramp: Ramp, horizontal: bool) {
    let gradient = if horizontal {
        cairo::LinearGradient::new(x, y, x + w, y)
    } else {
        cairo::LinearGradient::new(x, y + h, x, y)
    };
    gradient.add_color_stop_rgb(0.0, ramp.low.0, ramp.low.1, ramp.low.2);
    gradient.add_color_stop_rgb(1.0, ramp.high.0, ramp.high.1, ramp.high.2);
    cr.rectangle(x, y, w, h);
    let _ = cr.set_source(&gradient);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.5);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
}

fn paint_handle(cr: &cairo::Context, (cx, cy): (f64, f64), fill: Rgb, input: bool) {
    let half = HANDLE / 2.0;
    if input {
        cr.move_to(cx, cy - half);
        cr.line_to(cx + half, cy + half);
        cr.line_to(cx - half, cy + half);
    } else {
        cr.move_to(cx + half, cy);
        cr.line_to(cx - half, cy + half);
        cr.line_to(cx - half, cy - half);
    }
    cr.close_path();
    set_rgb(cr, fill);
    let _ = cr.fill_preserve();
    let luma = 0.3 * fill.0 + 0.59 * fill.1 + 0.11 * fill.2;
    let edge = if luma > 0.5 { 0.15 } else { 0.85 };
    cr.set_source_rgb(edge, edge, edge);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
}

fn set_rgb(cr: &cairo::Context, (r, g, b): Rgb) {
    cr.set_source_rgb(r, g, b);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_scale_ignores_two_spikes() {
        let mut bins = vec![10u32; LUT_SIZE];
        bins[0] = 5000;
        bins[255] = 9000;
        bins[100] = 20;
        let heights = histogram_heights(&bins);
        assert!((heights[100] - 1.0).abs() < 1e-9);
        assert!((heights[50] - 0.5).abs() < 1e-9);
        assert!((heights[0] - 1.0).abs() < 1e-9, "spikes clamp to full height");
    }

    #[test]
    fn layout_maps_levels_both_ways() {
        let l = Layout::new(300.0, 300.0, true, true);
        for level in [0u8, 1, 127, 128, 254, 255] {
            let p = CurvePoint::new(level, 255 - level);
            let (x, y) = l.position(p);
            assert_eq!(l.level_at(x, y), p);
        }
        assert!(l.contains(l.x + 1.0, l.y + 1.0));
        assert!((l.distance_outside(l.x - 30.0, l.y + 5.0) - 30.0).abs() < 1e-9);
        assert!(l.distance_outside(l.x + 5.0, l.y + 5.0).abs() < 1e-9);
    }
}
