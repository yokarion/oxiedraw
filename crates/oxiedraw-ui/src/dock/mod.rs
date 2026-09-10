//! Turns a layout tree into real widgets: a `gtk::Paned` per split, an empty
//! slot per panel. The panels belong to the open documents, so a rebuild fills
//! the new slots from whichever document is in front.

pub(crate) mod drop;
pub(crate) mod edit_overlay;
pub(crate) mod landing;
pub(crate) mod manager;
pub(crate) mod selector;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::glib;
use relm4::gtk::graphene;
use relm4::gtk::gsk;
use relm4::gtk::prelude::*;

use crate::layout::{Axis, Layout, LayoutNode, PanelId, SplitChild};

use self::edit_overlay::EditOverlay;

struct DockView {
    root: gtk::Widget,
    slots: HashMap<PanelId, gtk::Box>,
    splits: Vec<BuiltSplit>,
}

pub(crate) struct Handle {
    pub(crate) paned: gtk::Paned,
    pub(crate) axis: Axis,
    pub(crate) band: graphene::Rect,
    pub(crate) settle: Rc<dyn Fn()>,
}

const HANDLE_GRAB: f32 = 5.0;

struct Carried {
    widget: gtk::Widget,
    home: gtk::Widget,
    size: (f64, f64),
    request: (i32, i32),
    slot: Option<(gtk::Box, i32, i32)>,
}

pub(crate) struct DockHost {
    root: gtk::Overlay,
    container: gtk::Box,
    drag_layer: gtk::Fixed,
    carried: RefCell<Option<Carried>>,
    pub(crate) edit: EditOverlay,
    canvas: gtk::Widget,
    view: RefCell<DockView>,
    refill: RefCell<Option<Rc<dyn Fn(&Self)>>>,
    resized: ResizeSlot,
}

type ResizeSlot = Rc<RefCell<Option<Rc<dyn Fn(Vec<SplitChild>, i32)>>>>;

impl DockHost {
    pub(crate) fn new(layout: &Layout, canvas: &gtk::Widget) -> Rc<Self> {
        let container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .build();
        let resized: ResizeSlot = Rc::new(RefCell::new(None));
        let view = build_view(&layout.root, canvas, &resized);
        container.append(&view.root);

        let edit = EditOverlay::new();
        let root = gtk::Overlay::builder().child(&container).build();
        let drag_layer = gtk::Fixed::builder()
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::Fill)
            .can_target(false)
            .build();
        root.add_overlay(&drag_layer);
        root.add_overlay(edit.widget());

        Rc::new(Self {
            root,
            container,
            drag_layer,
            carried: RefCell::new(None),
            edit,
            canvas: canvas.clone(),
            view: RefCell::new(view),
            refill: RefCell::new(None),
            resized,
        })
    }

    pub(crate) fn set_resize_handler(&self, handler: Rc<dyn Fn(Vec<SplitChild>, i32)>) {
        *self.resized.borrow_mut() = Some(handler);
    }

    pub(crate) fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    pub(crate) fn leaves(&self) -> Vec<(LayoutNode, graphene::Rect)> {
        let view = self.view.borrow();
        let mut out = Vec::with_capacity(view.slots.len() + 1);
        for (id, slot) in &view.slots {
            if let Some(rect) = slot.compute_bounds(&self.container) {
                out.push((LayoutNode::Panel(*id), rect));
            }
        }
        if let Some(rect) = self.canvas.compute_bounds(&self.container) {
            out.push((LayoutNode::Canvas, rect));
        }
        out
    }

    pub(crate) fn panel_widget(&self, id: PanelId) -> Option<gtk::Widget> {
        self.view.borrow().slots.get(&id)?.first_child()
    }

    pub(crate) fn carry_begin(&self, widget: &gtk::Widget) -> Option<graphene::Rect> {
        self.carry_end();
        let bounds = widget.compute_bounds(&self.container)?;
        let home = widget.parent()?;
        if !home.is::<gtk::Box>() && !home.is::<gtk::Overlay>() {
            tracing::warn!(parent = %home.type_().name(), "refusing to carry a panel out of a parent it cannot be put back into");
            return None;
        }
        let slot = home.downcast_ref::<gtk::Box>().map(|slot| {
            let previous = (slot.width_request(), slot.height_request());
            slot.set_size_request(slot.width(), slot.height());
            (slot.clone(), previous.0, previous.1)
        });

        let request = (widget.width_request(), widget.height_request());
        detach(widget);
        widget.set_size_request(widget_width(&bounds), widget_height(&bounds));
        self.drag_layer.put(widget, 0.0, 0.0);
        *self.carried.borrow_mut() = Some(Carried {
            widget: widget.clone(),
            home,
            size: (f64::from(bounds.width()), f64::from(bounds.height())),
            request,
            slot,
        });
        Some(bounds)
    }

    pub(crate) fn carry_to(&self, rect: graphene::Rect) {
        let carried = self.carried.borrow();
        let Some(carried) = carried.as_ref() else {
            return;
        };
        if carried.size.0 <= 0.0 || carried.size.1 <= 0.0 {
            return;
        }
        let transform = gsk::Transform::new()
            .translate(&graphene::Point::new(rect.x(), rect.y()))
            .scale(
                rect.width() / carried.size.0 as f32,
                rect.height() / carried.size.1 as f32,
            );
        self.drag_layer
            .set_child_transform(&carried.widget, Some(&transform));
    }

    pub(crate) fn carry_end(&self) {
        let Some(carried) = self.carried.borrow_mut().take() else {
            return;
        };
        tracing::trace!(target: "oxiedraw::layout", "carried panel put back");
        self.drag_layer.set_child_transform(&carried.widget, None);
        detach(&carried.widget);
        carried
            .widget
            .set_size_request(carried.request.0, carried.request.1);
        if let Some((slot, width, height)) = carried.slot {
            slot.set_size_request(width, height);
        }
        if let Some(slot) = carried.home.downcast_ref::<gtk::Box>() {
            slot.append(&carried.widget);
        } else if let Some(overlay) = carried.home.downcast_ref::<gtk::Overlay>() {
            overlay.add_overlay(&carried.widget);
        }
    }

    pub(crate) fn bounds_of(&self, widget: &gtk::Widget) -> Option<graphene::Rect> {
        widget.compute_bounds(&self.container)
    }

    pub(crate) fn handles(&self) -> Vec<Handle> {
        let view = self.view.borrow();
        view.splits
            .iter()
            .filter(|split| split.resizable)
            .filter_map(|split| {
                let (paned, axis) = (&split.paned, &split.axis);
                let bounds = paned.compute_bounds(&self.container)?;
                #[allow(clippy::cast_precision_loss)]
                let seam = paned.position() as f32;
                let band = match axis {
                    Axis::Horizontal => graphene::Rect::new(
                        bounds.x() + seam - HANDLE_GRAB,
                        bounds.y(),
                        HANDLE_GRAB * 2.0,
                        bounds.height(),
                    ),
                    Axis::Vertical => graphene::Rect::new(
                        bounds.x(),
                        bounds.y() + seam - HANDLE_GRAB,
                        bounds.width(),
                        HANDLE_GRAB * 2.0,
                    ),
                };
                Some(Handle {
                    paned: paned.clone(),
                    axis: *axis,
                    band,
                    settle: Rc::clone(&split.settle),
                })
            })
            .collect()
    }

    pub(crate) fn set_refill(&self, refill: Rc<dyn Fn(&Self)>) {
        *self.refill.borrow_mut() = Some(refill);
    }

    pub(crate) fn fill(&self, id: PanelId, child: &gtk::Widget) {
        let Some(slot) = self.view.borrow().slots.get(&id).cloned() else {
            detach(child);
            return;
        };
        if child.parent().as_ref() == Some(slot.upcast_ref::<gtk::Widget>()) {
            return;
        }
        while let Some(existing) = slot.first_child() {
            slot.remove(&existing);
        }
        detach(child);
        slot.append(child);
    }

    pub(crate) fn rebuild(&self, layout: &Layout) {
        tracing::debug!(target: "oxiedraw::layout", layout = %layout.name, "dock rebuilt");
        self.carry_end();
        {
            let view = self.view.borrow();
            for slot in view.slots.values() {
                while let Some(child) = slot.first_child() {
                    slot.remove(&child);
                }
            }
            self.container.remove(&view.root);
            detach(&self.canvas);
        }

        let view = build_view(&layout.root, &self.canvas, &self.resized);
        self.container.append(&view.root);
        *self.view.borrow_mut() = view;

        let refill = self.refill.borrow().clone();
        if let Some(refill) = refill {
            refill(self);
        }
        self.edit.invalidate();
    }
}

fn build_view(root: &LayoutNode, canvas: &gtk::Widget, resized: &ResizeSlot) -> DockView {
    let mut built = Built::default();
    let root = build_node(root, canvas, &mut built, None, Vec::new(), resized);
    DockView {
        root,
        slots: built.slots,
        splits: built.splits,
    }
}

#[derive(Default)]
struct Built {
    slots: HashMap<PanelId, gtk::Box>,
    splits: Vec<BuiltSplit>,
}

struct BuiltSplit {
    paned: gtk::Paned,
    axis: Axis,
    resizable: bool,
    settle: Rc<dyn Fn()>,
}

fn build_node(
    node: &LayoutNode,
    canvas: &gtk::Widget,
    built: &mut Built,
    axis: Option<Axis>,
    path: Vec<SplitChild>,
    resized: &ResizeSlot,
) -> gtk::Widget {
    match node {
        LayoutNode::Canvas => canvas.clone(),
        LayoutNode::Panel(id) => build_slot(*id, axis, &mut built.slots),
        LayoutNode::Split {
            axis,
            fixed,
            size,
            first,
            second,
        } => {
            let paned = gtk::Paned::builder()
                .orientation(orientation(*axis))
                .resize_start_child(*fixed != SplitChild::First)
                .resize_end_child(*fixed != SplitChild::Second)
                .shrink_start_child(false)
                .shrink_end_child(false)
                .wide_handle(false)
                .hexpand(true)
                .vexpand(true)
                .build();
            let mut first_path = path.clone();
            first_path.push(SplitChild::First);
            let mut second_path = path.clone();
            second_path.push(SplitChild::Second);
            let first_widget = build_node(first, canvas, built, Some(*axis), first_path, resized);
            let second_widget = build_node(second, canvas, built, Some(*axis), second_path, resized);
            paned.set_start_child(Some(&first_widget));
            paned.set_end_child(Some(&second_widget));

            let (fixed_widget, fixed_node) = if *fixed == SplitChild::First {
                (&first_widget, first.as_ref())
            } else {
                (&second_widget, second.as_ref())
            };
            let panel = match fixed_node {
                LayoutNode::Panel(id) => Some(*id),
                _ => None,
            };
            let settle = install_sizing(
                &paned,
                fixed_widget,
                Split {
                    axis: *axis,
                    fixed: *fixed,
                    size: *size,
                    panel,
                    path,
                },
                Rc::clone(resized),
            );
            disable_handle(&paned);
            built.splits.push(BuiltSplit {
                paned: paned.clone(),
                axis: *axis,
                resizable: panel.is_none_or(|id| id.spec().resizable),
                settle,
            });
            paned.upcast()
        }
    }
}

fn build_slot(
    id: PanelId,
    axis: Option<Axis>,
    slots: &mut HashMap<PanelId, gtk::Box>,
) -> gtk::Widget {
    let slot = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    if let Some(axis) = axis {
        set_request(slot.upcast_ref::<gtk::Widget>(), axis, id.spec().min_size);
    }
    slots.insert(id, slot.clone());
    slot.upcast()
}

const fn orientation(axis: Axis) -> gtk::Orientation {
    match axis {
        Axis::Horizontal => gtk::Orientation::Horizontal,
        Axis::Vertical => gtk::Orientation::Vertical,
    }
}

fn set_request(widget: &gtk::Widget, axis: Axis, px: i32) {
    let px = px.max(-1);
    match axis {
        Axis::Horizontal => widget.set_width_request(px),
        Axis::Vertical => widget.set_height_request(px),
    }
}

fn request(widget: &gtk::Widget, axis: Axis) -> i32 {
    match axis {
        Axis::Horizontal => widget.width_request(),
        Axis::Vertical => widget.height_request(),
    }
}

struct Split {
    axis: Axis,
    fixed: SplitChild,
    size: i32,
    panel: Option<PanelId>,
    path: Vec<SplitChild>,
}

struct Sizing {
    split: Split,
    fixed_widget: gtk::Widget,
    handle: Cell<i32>,
    minimum: i32,
    adjusting: Cell<bool>,
    length: Cell<i32>,
}

impl Sizing {
    fn settle(&self, paned: &gtk::Paned) -> bool {
        if self.handle.get() != UNMEASURED {
            return true;
        }
        let (axis, fixed, size) = (self.split.axis, self.split.fixed, self.split.size);
        let Some(measured) = measure_handle(paned, axis) else {
            return false;
        };
        let Some(position) = position_for(paned, axis, fixed, size, measured) else {
            return false;
        };
        self.handle.set(measured);
        self.length.set(length(paned, axis));
        self.move_seam(paned, position);
        set_request(&self.fixed_widget, axis, self.minimum);
        true
    }

    fn move_seam(&self, paned: &gtk::Paned, position: i32) {
        self.adjusting.set(true);
        paned.set_position(position);
        self.adjusting.set(false);
    }

    fn seam_moved(&self, paned: &gtk::Paned) -> bool {
        let now = length(paned, self.split.axis);
        self.length.replace(now) == now
    }
}

const SETTLE_FRAMES: u32 = 120;

// Sizes come from the split's POSITION, never the children's allocations: an
// allocation is a frame behind a moving seam.
fn install_sizing(
    paned: &gtk::Paned,
    fixed_widget: &gtk::Widget,
    split: Split,
    resized: ResizeSlot,
) -> Rc<dyn Fn()> {
    let minimum = request(fixed_widget, split.axis);
    set_request(fixed_widget, split.axis, split.size);
    let sizing = Rc::new(Sizing {
        split,
        fixed_widget: fixed_widget.clone(),
        handle: Cell::new(UNMEASURED),
        minimum,
        adjusting: Cell::new(false),
        length: Cell::new(0),
    });

    {
        let sizing = Rc::clone(&sizing);
        let frames = Cell::new(0_u32);
        paned.add_tick_callback(move |paned, _| {
            frames.set(frames.get() + 1);
            if sizing.settle(paned) || frames.get() >= SETTLE_FRAMES {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    let settle = {
        let sizing = Rc::clone(&sizing);
        let paned = paned.clone();
        Rc::new(move || {
            if sizing.settle(&paned) {
                set_request(&sizing.fixed_widget, sizing.split.axis, sizing.minimum);
            }
        })
    };

    let sizing = Rc::clone(&sizing);
    paned.connect_position_notify(move |paned| {
        if sizing.adjusting.get() || !sizing.settle(paned) || !sizing.seam_moved(paned) {
            return;
        }
        let split = &sizing.split;
        let Some(size) = size_of(paned, split.axis, split.fixed, sizing.handle.get()) else {
            return;
        };
        let locked = split.panel.is_some_and(|id| !id.spec().resizable);
        let wanted = if locked {
            split.size
        } else {
            split.panel.map_or(size, |id| id.spec().snap(size))
        };
        if wanted != size
            && let Some(position) =
                position_for(paned, split.axis, split.fixed, wanted, sizing.handle.get())
        {
            sizing.move_seam(paned, position);
        }
        if locked {
            return;
        }
        let handler = resized.borrow().clone();
        if let Some(handler) = handler {
            handler(split.path.clone(), wanted);
        }
    });

    settle
}

const UNMEASURED: i32 = -1;

fn size_of(paned: &gtk::Paned, axis: Axis, fixed: SplitChild, handle: i32) -> Option<i32> {
    if fixed == SplitChild::First {
        return Some(paned.position().max(0));
    }
    let total = length(paned, axis);
    if total <= 0 {
        return None;
    }
    Some((total - handle - paned.position()).max(0))
}

fn position_for(
    paned: &gtk::Paned,
    axis: Axis,
    fixed: SplitChild,
    size: i32,
    handle: i32,
) -> Option<i32> {
    if fixed == SplitChild::First {
        return Some(size.max(0));
    }
    let total = length(paned, axis);
    if total <= 0 {
        return None;
    }
    Some((total - handle - size).max(0))
}

fn measure_handle(paned: &gtk::Paned, axis: Axis) -> Option<i32> {
    let measure = |widget: Option<gtk::Widget>| match axis {
        Axis::Horizontal => widget.map_or(0, |w| w.width()),
        Axis::Vertical => widget.map_or(0, |w| w.height()),
    };
    let total = length(paned, axis);
    let start = measure(paned.start_child());
    let end = measure(paned.end_child());
    if total <= 0 || start + end <= 0 || total - start - end < 0 {
        return None;
    }
    Some(total - start - end)
}

fn length(paned: &gtk::Paned, axis: Axis) -> i32 {
    match axis {
        Axis::Horizontal => paned.width(),
        Axis::Vertical => paned.height(),
    }
}

fn disable_handle(paned: &gtk::Paned) {
    silence(paned.upcast_ref::<gtk::Widget>());

    let (start, end) = (paned.start_child(), paned.end_child());
    let mut child = paned.first_child();
    while let Some(widget) = child {
        if start.as_ref() != Some(&widget) && end.as_ref() != Some(&widget) {
            silence(&widget);
            widget.set_cursor(None);
            widget.set_can_target(false);
            break;
        }
        child = widget.next_sibling();
    }
}

fn silence(widget: &gtk::Widget) {
    let controllers = widget.observe_controllers();
    for index in 0..controllers.n_items() {
        if let Some(controller) = controllers.item(index).and_downcast::<gtk::EventController>() {
            controller.set_propagation_phase(gtk::PropagationPhase::None);
        }
    }
}

fn detach(widget: &gtk::Widget) {
    let Some(parent) = widget.parent() else {
        return;
    };
    if let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
        if paned.start_child().as_ref() == Some(widget) {
            paned.set_start_child(None::<&gtk::Widget>);
        } else {
            paned.set_end_child(None::<&gtk::Widget>);
        }
    } else if let Some(container) = parent.downcast_ref::<gtk::Box>() {
        container.remove(widget);
    } else if let Some(fixed) = parent.downcast_ref::<gtk::Fixed>() {
        fixed.remove(widget);
    } else if let Some(overlay) = parent.downcast_ref::<gtk::Overlay>() {
        overlay.remove_overlay(widget);
    } else {
        widget.unparent();
    }
}

fn widget_width(bounds: &graphene::Rect) -> i32 {
    bounds.width().round() as i32
}

fn widget_height(bounds: &graphene::Rect) -> i32 {
    bounds.height().round() as i32
}
