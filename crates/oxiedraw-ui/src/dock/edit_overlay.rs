// One cairo-drawn layer over the dock, where panels are moved, resized and
// removed. Input is a legacy controller, not a `GestureDrag`: lifting a panel
// unparents it, and GTK cancels a gesture on the re-layout that follows.

use std::cell::{Cell, RefCell};
use std::f64::consts::TAU;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::cairo;
use relm4::gtk::glib;
use relm4::gtk::graphene;
use relm4::gtk::prelude::*;

use crate::layout::{
    Axis, FloatAnchor, FloatSize, Layout, LayoutNode, MAX_FLOAT_SIZE, MIN_FLOAT_SIZE, PanelId,
    ToolWindowId,
};
use crate::theme;

use super::Handle;
use super::drop::{self, DropHint, DropTarget};
use super::landing::{draw as draw_landing, rounded_rect};

const GRIP_RADIUS: f64 = 26.0;
const DELETE_RADIUS: f64 = 14.0;
const DISC_GAP: f64 = 8.0;
const VEIL_ALPHA: f64 = 0.45;
const VEIL_LIFTED: f64 = 0.82;
const GRIP_GREY: f64 = 0.62;
const GRIP_ALPHA: f64 = 0.62;
const FLOAT_MARGIN: f32 = 12.0;
const FLOAT_BAND: f32 = 7.0;
const EDGE_LINE: f64 = 3.0;
const EDGE_IDLE: f64 = 0.35;

pub(crate) enum EditRequest {
    Remove(PanelId),
    Move { panel: PanelId, target: DropTarget },
    Anchor {
        window: ToolWindowId,
        anchor: FloatAnchor,
    },
    Resize {
        window: ToolWindowId,
        size: FloatSize,
    },
}

pub(crate) struct EditHooks {
    pub(crate) layout: Rc<dyn Fn() -> Layout>,
    pub(crate) leaves: Rc<dyn Fn() -> Vec<(LayoutNode, graphene::Rect)>>,
    pub(crate) floats: Rc<dyn Fn() -> Vec<(ToolWindowId, graphene::Rect, FloatAnchor)>>,
    pub(crate) float_natural: Rc<dyn Fn(ToolWindowId) -> Option<(f32, f32)>>,
    pub(crate) float_resize: Rc<dyn Fn(ToolWindowId, FloatSize)>,
    pub(crate) handles: Rc<dyn Fn() -> Vec<Handle>>,
    pub(crate) carry_begin: Rc<dyn Fn(Movable) -> Option<graphene::Rect>>,
    pub(crate) carry_to: Rc<dyn Fn(graphene::Rect)>,
    pub(crate) carry_end: Rc<dyn Fn()>,
    pub(crate) request: Rc<dyn Fn(EditRequest)>,
}

#[derive(Clone, Copy)]
struct FloatResize {
    window: ToolWindowId,
    grip: (i8, i8),
    from_size: (f32, f32),
    stored: FloatSize,
    from: (f64, f64),
}

#[derive(Clone, Copy, PartialEq)]
struct FloatEdges {
    window: ToolWindowId,
    rect: graphene::Rect,
    free: (i8, i8),
    under: (i8, i8),
}

struct Resizing {
    paned: gtk::Paned,
    axis: Axis,
    origin: i32,
    from: (f64, f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Movable {
    Panel(PanelId),
    Float(ToolWindowId),
}

#[derive(Clone, Copy)]
struct Carried {
    size: (f32, f32),
    origin: graphene::Rect,
    rect: graphene::Rect,
}

enum Hint {
    Panel(DropHint),
    Float {
        rect: graphene::Rect,
        anchor: FloatAnchor,
    },
}

const DRAG_SEED: f32 = 64.0;
const CARRY_SHRINK: f32 = 10.0;
const GLIDE: f32 = 0.3;

struct State {
    hooks: RefCell<Option<EditHooks>>,
    preview: Cell<Option<graphene::Rect>>,
    carried: Cell<Option<Carried>>,
    glide: RefCell<Option<gtk::TickCallbackId>>,
    hovered: Cell<Option<Movable>>,
    hovered_seam: RefCell<Option<graphene::Rect>>,
    pointer: Cell<(f64, f64)>,
    dragging: Cell<Option<Movable>>,
    float_natural: Cell<Option<(f32, f32)>>,
    drag_layout: RefCell<Option<Layout>>,
    float_resize: Cell<Option<FloatResize>>,
    float_edges: Cell<Option<FloatEdges>>,
    resizing: RefCell<Option<Resizing>>,
    hint: RefCell<Option<Hint>>,
    holder: RefCell<Option<gtk::gdk::Device>>,
    holder_logged: Cell<bool>,
}

impl State {
    // Pen and mouse are separate devices and both reach here, so during a drag
    // only the one holding the panel counts, or the panel shakes between them.
    fn held_by(&self, device: Option<&gtk::gdk::Device>) -> bool {
        let holder = self.holder.borrow();
        let Some(holder) = holder.as_ref() else {
            return true;
        };
        if device == Some(holder) {
            return true;
        }
        if !self.holder_logged.replace(true) {
            tracing::trace!(
                target: "oxiedraw::layout",
                holder = %holder.name(),
                other = ?device.map(|device| device.name().to_string()),
                "ignoring the other pointer for the rest of this drag"
            );
        }
        false
    }

    fn hovered_rect(&self) -> Option<graphene::Rect> {
        self.rect_of(self.hovered.get()?)
    }

    fn rect_of(&self, movable: Movable) -> Option<graphene::Rect> {
        let hooks = self.hooks.borrow();
        let hooks = hooks.as_ref()?;
        match movable {
            Movable::Panel(id) => (hooks.leaves)()
                .into_iter()
                .find(|(leaf, _)| *leaf == LayoutNode::Panel(id))
                .map(|(_, rect)| rect),
            Movable::Float(id) => (hooks.floats)()
                .into_iter()
                .find(|(window, ..)| *window == id)
                .map(|(_, rect, _)| rect),
        }
    }

    fn float_edges_at(&self, x: f64, y: f64) -> Option<FloatEdges> {
        let hooks = self.hooks.borrow();
        let hooks = hooks.as_ref()?;
        #[allow(clippy::cast_possible_truncation)]
        let point = graphene::Point::new(x as f32, y as f32);
        (hooks.floats)().into_iter().find_map(|(window, rect, anchor)| {
            let free = anchor.free_edges();
            let under = edges_under(point, rect, free);
            (under != (0, 0)).then_some(FloatEdges {
                window,
                rect,
                free,
                under,
            })
        })
    }

    fn float_resize_at(&self, x: f64, y: f64) -> Option<FloatResize> {
        let edges = self.float_edges_at(x, y)?;
        let stored = self
            .hooks
            .borrow()
            .as_ref()
            .map(|hooks| (hooks.layout)().float_size(edges.window))?;
        Some(FloatResize {
            window: edges.window,
            grip: edges.under,
            from_size: (edges.rect.width(), edges.rect.height()),
            stored,
            from: (x, y),
        })
    }

    fn resize_float(&self, window: ToolWindowId, size: FloatSize) {
        let apply = self
            .hooks
            .borrow()
            .as_ref()
            .map(|hooks| Rc::clone(&hooks.float_resize));
        if let Some(apply) = apply {
            apply(window, size);
        }
    }

    fn natural_of(&self, window: ToolWindowId) -> Option<(f32, f32)> {
        let ask = self
            .hooks
            .borrow()
            .as_ref()
            .map(|hooks| Rc::clone(&hooks.float_natural))?;
        ask(window)
    }

    fn canvas_rect(&self) -> Option<graphene::Rect> {
        let hooks = self.hooks.borrow();
        (hooks.as_ref()?.leaves)()
            .into_iter()
            .find(|(leaf, _)| *leaf == LayoutNode::Canvas)
            .map(|(_, rect)| rect)
    }

    fn carry_target(&self) -> Option<graphene::Rect> {
        let (width, height) = self.carried.get()?.size;
        let longest = width.max(height);
        let scale = if longest > CARRY_SHRINK * 2.0 {
            (longest - CARRY_SHRINK) / longest
        } else {
            1.0
        };
        let (width, height) = (width * scale, height * scale);
        let (x, y) = self.pointer.get();
        Some(graphene::Rect::new(
            x as f32 - width / 2.0,
            y as f32 - height / 2.0,
            width,
            height,
        ))
    }

    fn preview_target(&self) -> graphene::Rect {
        match self.hint.borrow().as_ref() {
            Some(Hint::Panel(hint)) => hint.rect,
            Some(Hint::Float { rect, .. }) => *rect,
            None => {
                let (x, y) = self.pointer.get();
                graphene::Rect::new(
                    x as f32 - DRAG_SEED / 2.0,
                    y as f32 - DRAG_SEED / 2.0,
                    DRAG_SEED,
                    DRAG_SEED,
                )
            }
        }
    }

    fn seam_band(&self) -> Option<graphene::Rect> {
        let resizing = self.resizing.borrow();
        let Some(resizing) = resizing.as_ref() else {
            return *self.hovered_seam.borrow();
        };
        let hooks = self.hooks.borrow();
        (hooks.as_ref()?.handles)()
            .into_iter()
            .find(|handle| handle.paned == resizing.paned)
            .map(|handle| handle.band)
    }

    fn handle_at(&self, x: f64, y: f64) -> Option<Handle> {
        let hooks = self.hooks.borrow();
        let point = graphene::Point::new(x as f32, y as f32);
        (hooks.as_ref()?.handles)()
            .into_iter()
            .find(|handle| handle.band.contains_point(&point))
    }

    fn movable_at(&self, x: f64, y: f64) -> Option<Movable> {
        let hooks = self.hooks.borrow();
        let hooks = hooks.as_ref()?;
        let point = graphene::Point::new(x as f32, y as f32);
        let float = (hooks.floats)()
            .into_iter()
            .find(|(_, rect, _)| rect.contains_point(&point))
            .map(|(id, ..)| Movable::Float(id));
        float.or_else(|| {
            (hooks.leaves)()
                .into_iter()
                .find_map(|(leaf, rect)| match leaf {
                    LayoutNode::Panel(id) if rect.contains_point(&point) => {
                        Some(Movable::Panel(id))
                    }
                    _ => None,
                })
        })
    }
}

pub(crate) struct EditOverlay {
    area: gtk::DrawingArea,
    state: Rc<State>,
}

impl EditOverlay {
    pub(crate) fn new() -> Self {
        let area = gtk::DrawingArea::builder().visible(false).build();
        let state = Rc::new(State {
            hooks: RefCell::new(None),
            preview: Cell::new(None),
            carried: Cell::new(None),
            glide: RefCell::new(None),
            hovered: Cell::new(None),
            hovered_seam: RefCell::new(None),
            pointer: Cell::new((0.0, 0.0)),
            dragging: Cell::new(None),
            float_natural: Cell::new(None),
            drag_layout: RefCell::new(None),
            float_resize: Cell::new(None),
            float_edges: Cell::new(None),
            resizing: RefCell::new(None),
            hint: RefCell::new(None),
            holder: RefCell::new(None),
            holder_logged: Cell::new(false),
        });

        {
            let state = Rc::clone(&state);
            area.set_draw_func(move |area, cr, width, height| {
                draw(area, cr, f64::from(width), f64::from(height), &state);
            });
        }
        install_motion(&area, &state);
        install_buttons(&area, &state);

        Self { area, state }
    }

    pub(crate) fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    pub(crate) fn set_hooks(&self, hooks: EditHooks) {
        *self.state.hooks.borrow_mut() = Some(hooks);
    }

    pub(crate) fn set_active(&self, active: bool) {
        if !active {
            self.state.hovered.set(None);
            self.state.dragging.set(None);
            self.state.float_natural.set(None);
            self.state.drag_layout.borrow_mut().take();
            self.state.float_resize.set(None);
            self.state.float_edges.set(None);
            *self.state.hovered_seam.borrow_mut() = None;
            *self.state.resizing.borrow_mut() = None;
            *self.state.hint.borrow_mut() = None;
            self.state.holder.borrow_mut().take();
            stop_glide(&self.state);
            self.area.set_cursor(None);
        }
        self.area.set_visible(active);
        self.area.queue_draw();
    }

    pub(crate) fn invalidate(&self) {
        self.state.hovered.set(None);
        *self.state.hovered_seam.borrow_mut() = None;
        self.state.float_edges.set(None);
        self.area.queue_draw();
    }
}

fn install_motion(area: &gtk::DrawingArea, state: &Rc<State>) {
    let motion = gtk::EventControllerMotion::new();
    {
        let state = Rc::clone(state);
        let area = area.clone();
        motion.connect_motion(move |controller, x, y| {
            if !state.held_by(controller.current_event_device().as_ref()) {
                return;
            }
            state.pointer.set((x, y));
            if let Some(resizing) = state.resizing.borrow().as_ref() {
                let along = match resizing.axis {
                    Axis::Horizontal => x - resizing.from.0,
                    Axis::Vertical => y - resizing.from.1,
                };
                #[allow(clippy::cast_possible_truncation)]
                resizing.paned.set_position(resizing.origin + along as i32);
                area.queue_draw();
                return;
            }
            if let Some(resize) = state.float_resize.get() {
                let size = resized(resize, (x, y), state.canvas_rect().map(float_room));
                state.resize_float(resize.window, size);
                return;
            }
            if let Some(movable) = state.dragging.get() {
                *state.hint.borrow_mut() = hint_for(&state, &area, movable, (x, y));
                keep_gliding(&area, &state);
                area.queue_draw();
                return;
            }
            let handle = state.handle_at(x, y);
            let edges = if handle.is_some() {
                None
            } else {
                state.float_edges_at(x, y)
            };
            let cursor = handle
                .as_ref()
                .map(|handle| match handle.axis {
                    Axis::Horizontal => "col-resize",
                    Axis::Vertical => "row-resize",
                })
                .or_else(|| edges.and_then(|edges| edge_cursor(edges.under)));
            area.set_cursor_from_name(cursor);
            let seam = handle.map(|handle| handle.band);
            let hovered = if seam.is_some() {
                None
            } else {
                state.movable_at(x, y)
            };
            let seam_changed = *state.hovered_seam.borrow() != seam;
            *state.hovered_seam.borrow_mut() = seam;
            let edges_changed = state.float_edges.replace(edges) != edges;
            if state.hovered.replace(hovered) == hovered && !seam_changed && !edges_changed {
                return;
            }
            area.queue_draw();
        });
    }
    {
        let state = Rc::clone(state);
        motion.connect_enter(move |_, x, y| {
            if state.dragging.get().is_none() && state.resizing.borrow().is_none() {
                state.pointer.set((x, y));
            }
        });
    }
    {
        let state = Rc::clone(state);
        let area = area.clone();
        motion.connect_leave(move |_| {
            if state.dragging.get().is_none() {
                state.hovered.set(None);
                state.float_edges.set(None);
                area.queue_draw();
            }
        });
    }
    area.add_controller(motion);
}

fn install_buttons(area: &gtk::DrawingArea, state: &Rc<State>) {
    let buttons = gtk::EventControllerLegacy::new();
    let state = Rc::clone(state);
    let owner = area.clone();
    buttons.connect_event(move |_, event| {
        let area = &owner;
        let Some(button) = event.downcast_ref::<gtk::gdk::ButtonEvent>() else {
            return glib::Propagation::Proceed;
        };
        if button.button() != gtk::gdk::BUTTON_PRIMARY {
            return glib::Propagation::Proceed;
        }
        if !state.held_by(event.device().as_ref()) {
            return glib::Propagation::Proceed;
        }
        let (x, y) = state.pointer.get();
        match event.event_type() {
            gtk::gdk::EventType::ButtonPress => {
                *state.holder.borrow_mut() = event.device();
                state.holder_logged.set(false);
                press(area, &state, x, y);
            }
            gtk::gdk::EventType::ButtonRelease => {
                release(area, &state, x, y);
                state.holder.borrow_mut().take();
            }
            _ => {}
        }
        glib::Propagation::Proceed
    });
    area.add_controller(buttons);
}

fn press(area: &gtk::DrawingArea, state: &Rc<State>, x: f64, y: f64) {
    if let Some(handle) = state.handle_at(x, y) {
        (handle.settle)();
        *state.resizing.borrow_mut() = Some(Resizing {
            origin: handle.paned.position(),
            paned: handle.paned,
            axis: handle.axis,
            from: (x, y),
        });
        return;
    }
    if let Some(resize) = state.float_resize_at(x, y) {
        state.float_resize.set(Some(resize));
        return;
    }
    let Some(movable) = state.movable_at(x, y) else {
        return;
    };
    let Some(rect) = state.rect_of(movable) else {
        return;
    };
    if !within(discs(rect).0, x, y) {
        return;
    }
    state.hovered.set(Some(movable));
    tracing::debug!(target: "oxiedraw::layout", "layout drag picked up");
    state.dragging.set(Some(movable));

    state.float_natural.set(match movable {
        Movable::Float(window) => state.natural_of(window),
        Movable::Panel(_) => None,
    });
    *state.drag_layout.borrow_mut() = state
        .hooks
        .borrow()
        .as_ref()
        .map(|hooks| (hooks.layout)());
    lift(state, movable);
    state.preview.set(Some(state.preview_target()));
    area.queue_draw();
}

fn release(area: &gtk::DrawingArea, state: &Rc<State>, x: f64, y: f64) {
    if state.resizing.borrow_mut().take().is_some() {
        *state.hovered_seam.borrow_mut() = None;
        area.queue_draw();
        return;
    }
    if let Some(resize) = state.float_resize.take() {
        let size = resized(resize, (x, y), state.canvas_rect().map(float_room));
        request(
            state,
            EditRequest::Resize {
                window: resize.window,
                size,
            },
        );
        area.queue_draw();
        return;
    }
    let Some(movable) = state.dragging.take() else {
        clicked(area, state, x, y);
        return;
    };
    tracing::debug!(target: "oxiedraw::layout", "layout drag let go");
    let hint = hint_for(state, area, movable, (x, y));
    state.float_natural.set(None);
    state.drag_layout.borrow_mut().take();
    *state.hint.borrow_mut() = None;
    state.hovered.set(None);
    stop_glide(state);
    area.queue_draw();

    match (movable, hint) {
        (_, Some(Hint::Panel(hint))) if hint.target == DropTarget::Keep => {}
        (Movable::Panel(panel), Some(Hint::Panel(hint))) => request(
            state,
            EditRequest::Move {
                panel,
                target: hint.target,
            },
        ),
        (Movable::Float(window), Some(Hint::Float { anchor, .. })) => {
            request(state, EditRequest::Anchor { window, anchor });
        }
        _ => {}
    }
}

fn clicked(area: &gtk::DrawingArea, state: &Rc<State>, x: f64, y: f64) {
    let Some(Movable::Panel(panel)) = state.movable_at(x, y) else {
        return;
    };
    let Some(rect) = state.rect_of(Movable::Panel(panel)) else {
        return;
    };
    let (_, delete) = discs(rect);
    if within(delete, x, y) && panel.spec().removable {
        state.hovered.set(None);
        area.queue_draw();
        request(state, EditRequest::Remove(panel));
    }
}

fn lift(state: &Rc<State>, movable: Movable) {
    let hooks = state.hooks.borrow();
    let Some(hooks) = hooks.as_ref() else { return };
    let Some(from) = (hooks.carry_begin)(movable) else {
        return;
    };
    (hooks.carry_to)(from);
    state.carried.set(Some(Carried {
        size: (from.width(), from.height()),
        origin: from,
        rect: from,
    }));
}

const SETTLED: f32 = 0.5;

fn keep_gliding(area: &gtk::DrawingArea, state: &Rc<State>) {
    if state.glide.borrow().is_some() {
        return;
    }
    start_glide(area, state);
}

fn start_glide(area: &gtk::DrawingArea, state: &Rc<State>) {
    cancel_tick(state);
    let state_for_tick = Rc::clone(state);
    let id = area.add_tick_callback(move |area, _| {
        let target = state_for_tick.preview_target();
        let current = state_for_tick.preview.get().unwrap_or(target);
        let mut moving = !arrived(current, target);
        state_for_tick.preview.set(Some(ease(current, target, GLIDE)));

        if let Some(target) = state_for_tick.carry_target()
            && let Some(mut carried) = state_for_tick.carried.get()
        {
            moving |= !arrived(carried.rect, target);
            carried.rect = ease(carried.rect, target, GLIDE);
            state_for_tick.carried.set(Some(carried));
            let hooks = state_for_tick.hooks.borrow();
            if let Some(hooks) = hooks.as_ref() {
                (hooks.carry_to)(carried.rect);
            }
        }

        if !moving {
            state_for_tick.glide.borrow_mut().take();
            return glib::ControlFlow::Break;
        }
        area.queue_draw();
        glib::ControlFlow::Continue
    });
    *state.glide.borrow_mut() = Some(id);
}

fn arrived(from: graphene::Rect, to: graphene::Rect) -> bool {
    let close = |a: f32, b: f32| (a - b).abs() < SETTLED;
    close(from.x(), to.x())
        && close(from.y(), to.y())
        && close(from.width(), to.width())
        && close(from.height(), to.height())
}

fn cancel_tick(state: &Rc<State>) {
    if let Some(id) = state.glide.borrow_mut().take() {
        id.remove();
    }
}

fn stop_glide(state: &Rc<State>) {
    cancel_tick(state);
    state.preview.set(None);
    if state.carried.take().is_some() {
        let hooks = state.hooks.borrow();
        if let Some(hooks) = hooks.as_ref() {
            (hooks.carry_end)();
        }
    }
}

fn ease(from: graphene::Rect, to: graphene::Rect, t: f32) -> graphene::Rect {
    let step = |a: f32, b: f32| (b - a).mul_add(t, a);
    graphene::Rect::new(
        step(from.x(), to.x()),
        step(from.y(), to.y()),
        step(from.width(), to.width()),
        step(from.height(), to.height()),
    )
}

fn edges_under(point: graphene::Point, rect: graphene::Rect, free: (i8, i8)) -> (i8, i8) {
    if !grown(rect, FLOAT_BAND).contains_point(&point) {
        return (0, 0);
    }
    let near = |edge: f32, at: f32| (at - edge).abs() <= FLOAT_BAND;
    let horizontal = match free.0 {
        1 if near(rect.x() + rect.width(), point.x()) => 1,
        -1 if near(rect.x(), point.x()) => -1,
        _ => 0,
    };
    let vertical = match free.1 {
        1 if near(rect.y() + rect.height(), point.y()) => 1,
        -1 if near(rect.y(), point.y()) => -1,
        _ => 0,
    };
    (horizontal, vertical)
}

fn grown(rect: graphene::Rect, by: f32) -> graphene::Rect {
    graphene::Rect::new(
        rect.x() - by,
        rect.y() - by,
        rect.width() + by * 2.0,
        rect.height() + by * 2.0,
    )
}

const fn edge_cursor(under: (i8, i8)) -> Option<&'static str> {
    match under {
        (0, 0) => None,
        (_, 0) => Some("ew-resize"),
        (0, _) => Some("ns-resize"),
        (h, v) if h * v > 0 => Some("nwse-resize"),
        _ => Some("nesw-resize"),
    }
}

fn resized(resize: FloatResize, at: (f64, f64), room: Option<(f32, f32)>) -> FloatSize {
    #[allow(clippy::cast_possible_truncation)]
    let (dx, dy) = ((at.0 - resize.from.0) as f32, (at.1 - resize.from.1) as f32);
    let mut size = resize.stored;
    if resize.grip.0 != 0 {
        let want = f32::from(resize.grip.0).mul_add(dx, resize.from_size.0);
        size.width = Some(clamp_axis(want, room.map(|room| room.0)));
    }
    if resize.grip.1 != 0 {
        let want = f32::from(resize.grip.1).mul_add(dy, resize.from_size.1);
        size.height = Some(clamp_axis(want, room.map(|room| room.1)));
    }
    size
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn clamp_axis(want: f32, room: Option<f32>) -> i32 {
    let floor = MIN_FLOAT_SIZE as f32;
    let ceiling = room.map_or(MAX_FLOAT_SIZE as f32, |room| room.max(floor));
    want.clamp(floor, ceiling) as i32
}

fn float_room(canvas: graphene::Rect) -> (f32, f32) {
    (
        canvas.width() - FLOAT_MARGIN * 2.0,
        canvas.height() - FLOAT_MARGIN * 2.0,
    )
}

fn request(state: &Rc<State>, req: EditRequest) {
    let handler = state
        .hooks
        .borrow()
        .as_ref()
        .map(|hooks| Rc::clone(&hooks.request));
    if let Some(handler) = handler {
        handler(req);
    }
}

fn hint_for(
    state: &Rc<State>,
    area: &gtk::DrawingArea,
    movable: Movable,
    point: (f64, f64),
) -> Option<Hint> {
    let point = (point.0 as f32, point.1 as f32);
    match movable {
        Movable::Panel(panel) => {
            let hooks = state.hooks.borrow();
            let hooks = hooks.as_ref()?;
            let layout = state.drag_layout.borrow();
            drop::hint_at(
                point,
                (area.width() as f32, area.height() as f32),
                &(hooks.leaves)(),
                layout.as_ref()?,
                panel,
            )
            .map(Hint::Panel)
        }
        Movable::Float(window) => {
            let canvas = state.canvas_rect()?;
            let size = if let Some(natural) = state.float_natural.get() {
                natural
            } else {
                let current = state.rect_of(Movable::Float(window))?;
                (current.width(), current.height())
            };
            let anchor = drop::anchor_at(point, canvas);
            let rect = drop::anchor_preview(canvas, size, anchor, FLOAT_MARGIN);
            Some(Hint::Float { rect, anchor })
        }
    }
}

type Disc = (f64, f64, f64);

fn discs(rect: graphene::Rect) -> (Disc, Disc) {
    let width = f64::from(rect.width());
    let height = f64::from(rect.height());
    let cx = f64::from(rect.x()) + width / 2.0;
    let cy = f64::from(rect.y()) + height / 2.0;
    let radius = GRIP_RADIUS.min(width / 2.0 - 2.0).min(height / 2.0 - 2.0).max(6.0);
    let delete_radius = DELETE_RADIUS.min(radius * 0.6).max(5.0);

    let stacked = height >= 2.0 * (radius + DISC_GAP + delete_radius);
    let delete = if stacked {
        (cx, cy - radius - DISC_GAP - delete_radius, delete_radius)
    } else {
        (cx + radius + DISC_GAP + delete_radius, cy, delete_radius)
    };
    let inside = |value: f64, low: f64, span: f64, margin: f64| {
        value.clamp(low + margin, (low + span - margin).max(low + margin))
    };
    let delete = (
        inside(delete.0, f64::from(rect.x()), width, delete_radius),
        inside(delete.1, f64::from(rect.y()), height, delete_radius),
        delete_radius,
    );
    ((cx, cy, radius), delete)
}

fn within((cx, cy, radius): Disc, x: f64, y: f64) -> bool {
    (x - cx).hypot(y - cy) <= radius
}

fn draw(area: &gtk::DrawingArea, cr: &cairo::Context, width: f64, height: f64, state: &Rc<State>) {
    let accent = theme::accent(area);

    if let Some(band) = state.seam_band() {
        let (x, y, w, h) = (
            f64::from(band.x()),
            f64::from(band.y()),
            f64::from(band.width()),
            f64::from(band.height()),
        );
        rounded_rect(cr, x, y, w, h, w.min(h) / 2.0);
        cr.set_source_rgba(accent.0, accent.1, accent.2, 0.9);
        let _ = cr.fill();
    }

    let hovered = state.hovered.get();
    let hovered_rect = state
        .carried
        .get()
        .map_or_else(|| state.hovered_rect(), |carried| Some(carried.origin));
    if let Some(rect) = hovered_rect {
        let dragging = state.dragging.get().is_some();
        let deletable = matches!(hovered, Some(Movable::Panel(id)) if id.spec().removable);
        draw_hover(cr, rect, theme::destructive(area), dragging, deletable);
    }

    if state.dragging.get().is_none()
        && let Some(edges) = state.float_edges.get()
    {
        draw_float_edges(cr, edges, accent);
    }

    if state.dragging.get().is_some() {
        let landing = state.hint.borrow().is_some();
        if !landing {
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.12);
            cr.rectangle(0.0, 0.0, width, height);
            let _ = cr.fill();
        }
        if let Some(rect) = state.preview.get() {
            draw_landing(cr, rect, accent, landing);
        }
        if state.carried.get().is_none() {
            let (x, y) = state.pointer.get();
            draw_grip(cr, (x, y, GRIP_RADIUS * 0.7), 0.9);
        }
    }
}

fn draw_float_edges(cr: &cairo::Context, edges: FloatEdges, accent: theme::Rgb) {
    let rect = edges.rect;
    let (left, top) = (f64::from(rect.x()), f64::from(rect.y()));
    let (right, bottom) = (left + f64::from(rect.width()), top + f64::from(rect.height()));
    let line = |x1: f64, y1: f64, x2: f64, y2: f64, lit: bool| {
        cr.set_source_rgba(accent.0, accent.1, accent.2, if lit { 0.9 } else { EDGE_IDLE });
        cr.set_line_width(EDGE_LINE);
        cr.move_to(x1, y1);
        cr.line_to(x2, y2);
        let _ = cr.stroke();
    };
    if edges.free.0 != 0 {
        let x = if edges.free.0 > 0 { right } else { left };
        line(x, top, x, bottom, edges.under.0 != 0);
    }
    if edges.free.1 != 0 {
        let y = if edges.free.1 > 0 { bottom } else { top };
        line(left, y, right, y, edges.under.1 != 0);
    }
}

fn draw_hover(
    cr: &cairo::Context,
    rect: graphene::Rect,
    red: theme::Rgb,
    dragging: bool,
    deletable: bool,
) {
    let veil = if dragging { VEIL_LIFTED } else { VEIL_ALPHA };
    cr.set_source_rgba(0.0, 0.0, 0.0, veil);
    cr.rectangle(
        f64::from(rect.x()),
        f64::from(rect.y()),
        f64::from(rect.width()),
        f64::from(rect.height()),
    );
    let _ = cr.fill();

    if dragging {
        return;
    }
    let (grip, delete) = discs(rect);
    draw_grip(cr, grip, 1.0);
    if deletable {
        draw_delete(cr, delete, red);
    }
}

fn draw_grip(cr: &cairo::Context, (cx, cy, radius): Disc, alpha: f64) {
    cr.set_source_rgba(GRIP_GREY, GRIP_GREY, GRIP_GREY, GRIP_ALPHA * alpha);
    cr.arc(cx, cy, radius, 0.0, TAU);
    let _ = cr.fill();

    let dot = (radius * 0.11).max(1.5);
    let step_x = radius * 0.34;
    let step_y = radius * 0.36;
    cr.set_source_rgba(1.0, 1.0, 1.0, alpha);
    for row in -1..=1 {
        for col in [-1.0, 1.0] {
            cr.arc(
                cx + col * step_x,
                cy + f64::from(row) * step_y,
                dot,
                0.0,
                TAU,
            );
            let _ = cr.fill();
        }
    }
}

fn draw_delete(cr: &cairo::Context, (cx, cy, radius): Disc, red: theme::Rgb) {
    cr.set_source_rgba(red.0, red.1, red.2, 0.95);
    cr.arc(cx, cy, radius, 0.0, TAU);
    let _ = cr.fill();
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.set_line_width((radius * 0.11).max(1.0));

    let half = radius * 0.42;
    cr.move_to(cx - half, cy - half * 0.55);
    cr.line_to(cx + half, cy - half * 0.55);
    let _ = cr.stroke();

    cr.move_to(cx - half * 0.72, cy - half * 0.55);
    cr.line_to(cx - half * 0.58, cy + half);
    cr.line_to(cx + half * 0.58, cy + half);
    cr.line_to(cx + half * 0.72, cy - half * 0.55);
    let _ = cr.stroke();

    for offset in [-half * 0.25, half * 0.25] {
        cr.move_to(cx + offset, cy - half * 0.2);
        cr.line_to(cx + offset, cy + half * 0.6);
    }
    let _ = cr.stroke();
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Corner;

    #[test]
    fn an_edge_is_grabbed_from_either_side_of_it() {
        let rect = graphene::Rect::new(700.0, 100.0, 300.0, 200.0);
        let free = FloatAnchor::Corner(Corner::TopRight).free_edges();
        let at = |x: f32, y: f32| edges_under(graphene::Point::new(x, y), rect, free);

        assert_eq!(at(701.0, 200.0), (-1, 0), "just inside the left edge");
        assert_eq!(at(697.0, 200.0), (-1, 0), "and just outside it");
        assert_eq!(at(850.0, 200.0), (0, 0), "the middle is for the grip");
        assert_eq!(at(999.0, 200.0), (0, 0), "the pinned edge does not give");
        assert_eq!(at(850.0, 299.0), (0, 0), "and a corner has no height to set");
    }

    #[test]
    fn a_resize_runs_with_the_pointer_and_stops_at_the_canvas() {
        let resize = FloatResize {
            window: ToolWindowId::Crop,
            grip: (-1, 0),
            from_size: (300.0, 200.0),
            stored: FloatSize::default(),
            from: (700.0, 200.0),
        };
        let room = Some((900.0, 600.0));

        let wider = resized(resize, (600.0, 200.0), room);
        assert_eq!(wider.width, Some(400));
        assert_eq!(wider.height, None, "an axis nobody dragged is left alone");

        let narrower = resized(resize, (900.0, 200.0), room);
        assert_eq!(narrower.width, Some(MIN_FLOAT_SIZE.max(100)));

        let capped = resized(resize, (-4000.0, 200.0), room);
        assert_eq!(capped.width, Some(900));
        let floored = resized(resize, (4000.0, 200.0), room);
        assert_eq!(floored.width, Some(MIN_FLOAT_SIZE));
    }

    #[test]
    fn resizing_keeps_the_size_stored_for_the_other_axis() {
        let resize = FloatResize {
            window: ToolWindowId::Crop,
            grip: (0, 1),
            from_size: (300.0, 200.0),
            stored: FloatSize {
                width: Some(420),
                height: None,
            },
            from: (800.0, 300.0),
        };
        let taller = resized(resize, (800.0, 350.0), Some((900.0, 600.0)));
        assert_eq!(taller.height, Some(250));
        assert_eq!(taller.width, Some(420), "the width it already had");
    }
}
