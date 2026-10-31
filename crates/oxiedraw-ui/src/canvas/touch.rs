//! Touchscreen input: finger pan, pinch-zoom, rotate, taps and one-finger drawing.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use oxiedraw_utils::geometry::Point;
use oxiedraw_utils::math::wrap_pi;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use super::{MAX_ZOOM, MIN_ZOOM, QuickMenuSlot, ViewSync, Viewport, widget_to_canvas_xf};
use crate::input::{self, Navigation, TouchAction, TouchPlan};

const TAP_MAX_DURATION: Duration = Duration::from_millis(250);
const TAP_SLOP_PX: f32 = 12.0;
// Below this finger spread the angle is too noisy to rotate by.
const MIN_SPAN_PX: f32 = 8.0;
// How far the fingers must turn before a pinch or pan starts rotating too.
const ROTATE_START_RAD: f32 = 10.0 * std::f32::consts::PI / 180.0;
// Widest pull of a snap step; narrower for small steps so free angles remain.
const SNAP_MAGNET_RAD: f32 = 5.0 * std::f32::consts::PI / 180.0;


#[derive(Clone, Copy, Debug, PartialEq)]
struct View {
    pan: Point,
    zoom: f32,
    rotation: f32,
}

impl View {
    fn read(sync: &ViewSync) -> Self {
        Self {
            pan: sync.pan.get(),
            zoom: sync.zoom.get(),
            rotation: sync.rotation.get(),
        }
    }

    fn write(self, sync: &ViewSync) {
        sync.pan.set(self.pan);
        sync.zoom.set(self.zoom);
        sync.rotation.set(self.rotation);
        sync.commit();
    }
}

struct Finger {
    sequence: gdk::EventSequence,
    start: Point,
    pos: Point,
}

#[derive(Clone, Copy)]
struct Snap {
    mask: Option<gdk::ModifierType>,
    step: f32,
}

struct Session {
    began: Instant,
    max_fingers: usize,
    // A finger left the tap slop; the view only follows from then on.
    moved: bool,
    // The pen was down at some point: these are palm contacts, so the whole
    // session is ignored until every finger lifts.
    palm: bool,
    // Turn gathered before rotation engages, and whether it has.
    pending_turn: f32,
    rotating: bool,
    // Unsnapped angle the fingers have turned the view to; snapping rounds
    // this, so small turns still add up to the next step.
    free_rotation: f32,
    // Read from settings once rotation engages, keeping disk off the
    // touch-down path.
    snap: Option<Snap>,
    plan: TouchPlan,
    landed_sum: Point,
    landed_count: usize,
}

impl Session {
    fn new(rotation: f32, plan: TouchPlan) -> Self {
        Self {
            began: Instant::now(),
            max_fingers: 0,
            moved: false,
            palm: false,
            pending_turn: 0.0,
            rotating: false,
            free_rotation: rotation,
            snap: None,
            plan,
            landed_sum: Point::ZERO,
            landed_count: 0,
        }
    }

    fn note_landing(&mut self, pos: Point) {
        self.landed_sum = Point::new(self.landed_sum.x + pos.x, self.landed_sum.y + pos.y);
        self.landed_count += 1;
    }

    fn tapped(&self) -> Option<TouchAction> {
        if self.moved || self.palm || self.began.elapsed() > TAP_MAX_DURATION {
            return None;
        }
        self.plan.tap(self.max_fingers)
    }

    fn tap_point(&self) -> Point {
        #[allow(clippy::cast_precision_loss)]
        let count = self.landed_count.max(1) as f32;
        Point::new(self.landed_sum.x / count, self.landed_sum.y / count)
    }

    // Swallows the turn until it passes the start threshold; from the next
    // event on rotation follows, so engaging never jumps.
    fn add_turn(&mut self, turn: f32) {
        if self.rotating {
            self.free_rotation += turn;
            return;
        }
        self.pending_turn += turn;
        self.rotating = self.pending_turn.abs() >= ROTATE_START_RAD;
        if self.rotating && self.snap.is_none() {
            self.snap = Some(Snap {
                mask: super::drag_modifier_mask("rotate-snap-modifier"),
                step: super::rotation_snap_rad(),
            });
        }
    }

    fn shown_rotation(&self, modifiers: gdk::ModifierType) -> f32 {
        let Some(snap) = self.snap.filter(|_| self.rotating) else {
            return self.free_rotation;
        };
        let nearest = (self.free_rotation / snap.step).round() * snap.step;
        let forced = snap.mask.is_some_and(|mask| modifiers.contains(mask));
        let magnet = (snap.step * 0.25).min(SNAP_MAGNET_RAD);
        if forced || (self.free_rotation - nearest).abs() <= magnet {
            nearest
        } else {
            self.free_rotation
        }
    }
}

struct DrawingFinger {
    finger: Finger,
    landed: Instant,
}

impl DrawingFinger {
    fn is_young(&self) -> bool {
        is_stray_dab(self.landed.elapsed(), self.finger.start, self.finger.pos)
    }
}

pub(super) fn is_stray_dab(age: Duration, start: Point, pos: Point) -> bool {
    age <= TAP_MAX_DURATION && start.distance(pos) <= TAP_SLOP_PX
}

struct TouchState {
    fingers: Vec<Finger>,
    session: Option<Session>,
    drawing: Option<DrawingFinger>,
    plan: TouchPlan,
}

impl TouchState {
    // The first two fingers steer; one finger alone is a degenerate pair.
    fn steering_pair(&self) -> Option<(Point, Point)> {
        let first = self.fingers.first()?.pos;
        let second = self.fingers.get(1).map_or(first, |f| f.pos);
        Some((first, second))
    }

    fn landing_pair(&self) -> Option<(Point, Point)> {
        let first = self.fingers.first()?.start;
        let second = self.fingers.get(1).map_or(first, |f| f.start);
        Some((first, second))
    }

    // Drops fingers whose lift never arrived, so they can't pin the render
    // pump or steer later gestures.
    fn reset(&mut self, pump: &super::RenderPump) {
        self.fingers.clear();
        self.drawing = None;
        if self.session.take().is_some() {
            pump.disarm();
        }
    }
}

enum Outcome {
    Pass,
    Consumed,
    HandOver { abandon: bool },
    Tap(TouchAction, Point),
}

struct Context<'a> {
    sync: &'a ViewSync,
    pump: &'a super::RenderPump,
    pen_down: bool,
    fresh: bool,
    modifiers: gdk::ModifierType,
}


pub(super) fn install_touch_nav(
    area: &gtk::Picture,
    viewport: &Viewport,
    primary_drag: &gtk::GestureDrag,
) {
    let legacy = gtk::EventControllerLegacy::new();
    // Capture phase, consuming every touch event it navigates with, so those
    // fingers never reach the drawing gesture on the same widget.
    legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
    let state = Rc::new(RefCell::new(TouchState {
        fingers: Vec::new(),
        session: None,
        drawing: None,
        plan: input::read(|s| s.touchscreen.plan()),
    }));
    {
        // A tab switch can unmap the canvas between a finger's press and its lift.
        let state = Rc::clone(&state);
        let pump = viewport.render_pump();
        area.connect_unmap(move |_| state.borrow_mut().reset(&pump));
    }
    let sync = Rc::new(viewport.view_sync());
    let pump = viewport.render_pump();
    let animator = viewport.rotation_animator();
    let quick_menu = Rc::clone(&viewport.quick_menu);
    let abandon_stroke = Rc::clone(&viewport.abandon_stroke);
    // Weak: the picture owns both controllers.
    let primary_drag = primary_drag.downgrade();
    let area_c = area.clone();
    legacy.connect_event(move |_, event| {
        let kind = event.event_type();
        if !matches!(
            kind,
            gdk::EventType::TouchBegin
                | gdk::EventType::TouchUpdate
                | gdk::EventType::TouchEnd
                | gdk::EventType::TouchCancel
        ) {
            return glib::Propagation::Proceed;
        }
        #[allow(clippy::cast_possible_truncation)]
        let pos = crate::brush_popup::event_position_in(&area_c, event)
            .map(|(x, y)| Point::new(x as f32, y as f32));
        let primary = primary_drag.upgrade();
        let outcome = {
            let mut state = state.borrow_mut();
            // GTK's own gesture state, so a denied or lost pen drag can't leave
            // touch navigation switched off.
            let pen_down = state.drawing.is_none() && primary.as_ref().is_some_and(GestureExt::is_active);
            // The first finger of a fresh contact emulates the pointer; any
            // fingers still tracked then are stale.
            let fresh = kind == gdk::EventType::TouchBegin
                && event
                    .downcast_ref::<gdk::TouchEvent>()
                    .is_some_and(gdk::TouchEvent::is_emulating_pointer);
            if fresh {
                state.reset(&pump);
                state.plan = input::read(|s| s.touchscreen.plan());
            }
            if fresh && state.session.is_none() {
                animator.cancel();
            }
            let context = Context { sync: &sync, pump: &pump, pen_down, fresh, modifiers: event.modifier_state() };
            handle(&mut state, kind, event.event_sequence(), pos, &context)
        };
        match outcome {
            Outcome::Pass => return glib::Propagation::Proceed,
            Outcome::Consumed => {}
            Outcome::HandOver { abandon } => {
                if let Some(drag) = primary {
                    abandon_stroke.set(abandon);
                    drag.set_state(gtk::EventSequenceState::Denied);
                    abandon_stroke.set(false);
                }
            }
            Outcome::Tap(action, at) => run_tap(&area_c, &quick_menu, action, at),
        }
        glib::Propagation::Stop
    });
    area.add_controller(legacy);
}

fn handle(
    state: &mut TouchState,
    kind: gdk::EventType,
    sequence: gdk::EventSequence,
    pos: Option<Point>,
    context: &Context,
) -> Outcome {
    if let Some(drawing) = state.drawing.as_mut().filter(|d| d.finger.sequence == sequence) {
        match kind {
            gdk::EventType::TouchUpdate => {
                if let Some(pos) = pos {
                    drawing.finger.pos = pos;
                }
            }
            gdk::EventType::TouchEnd | gdk::EventType::TouchCancel => state.drawing = None,
            _ => {}
        }
        return Outcome::Pass;
    }
    // Fix: any touch while the pen is down marks the session as palm, not just a new finger.
    if context.pen_down && let Some(session) = state.session.as_mut() {
        session.palm = true;
    }
    match (kind, pos) {
        (gdk::EventType::TouchBegin, Some(pos)) => {
            if context.fresh && state.plan.draw && !context.pen_down {
                let finger = Finger { sequence, start: pos, pos };
                state.drawing = Some(DrawingFinger { finger, landed: Instant::now() });
                return Outcome::Pass;
            }
            let handed = state.drawing.take();
            let young = handed.as_ref().is_some_and(DrawingFinger::is_young);
            if let Some(drawing) = &handed {
                let at = if young { drawing.finger.start } else { drawing.finger.pos };
                press(state, drawing.finger.sequence.clone(), at, context);
                if young && let Some(session) = state.session.as_mut() {
                    session.began = drawing.landed;
                }
            }
            press(state, sequence, pos, context);
            if let Some(session) = state.session.as_mut() {
                session.palm |= context.pen_down;
                session.moved |= handed.is_some() && !young;
            }
            if handed.is_some() { Outcome::HandOver { abandon: young } } else { Outcome::Consumed }
        }
        (gdk::EventType::TouchUpdate, Some(pos)) => {
            drag(state, &sequence, pos, context);
            Outcome::Consumed
        }
        (gdk::EventType::TouchEnd | gdk::EventType::TouchCancel, _) => {
            let cancelled = kind == gdk::EventType::TouchCancel;
            release(state, &sequence, cancelled, context.pump)
                .map_or(Outcome::Consumed, |(action, at)| Outcome::Tap(action, at))
        }
        _ => Outcome::Consumed,
    }
}

fn run_tap(area: &gtk::Picture, quick_menu: &QuickMenuSlot, action: TouchAction, at: Point) {
    let name = match action {
        TouchAction::Undo => "app.undo",
        TouchAction::Redo => "app.redo",
        TouchAction::QuickMenu => {
            let open = quick_menu.borrow().clone();
            if let Some(open) = open {
                open(f64::from(at.x), f64::from(at.y));
            }
            return;
        }
        TouchAction::Draw | TouchAction::Pan | TouchAction::Rotate => return,
    };
    if let Err(e) = area.activate_action(name, None) {
        tracing::warn!(%e, name, "touch tap: action missing");
    }
}

fn press(state: &mut TouchState, sequence: gdk::EventSequence, pos: Point, context: &Context) {
    if state.session.is_none() {
        context.pump.arm();
        state.session = Some(Session::new(context.sync.rotation.get(), state.plan));
    }
    state.fingers.push(Finger { sequence, start: pos, pos });
    let count = state.fingers.len();
    if let Some(session) = state.session.as_mut() {
        session.max_fingers = session.max_fingers.max(count);
        session.note_landing(pos);
    }
}

fn drag(state: &mut TouchState, sequence: &gdk::EventSequence, pos: Point, context: &Context) {
    let engaged = state.session.as_ref().is_some_and(|s| s.moved);
    // Until a finger leaves the slop the view stays put, so a tap never nudges
    // it; the first real move then catches up from where the fingers landed.
    let before = if engaged {
        state.steering_pair()
    } else {
        state.landing_pair()
    };
    let Some(finger) = state.fingers.iter_mut().find(|f| &f.sequence == sequence) else {
        return;
    };
    finger.pos = pos;
    if !engaged && finger.start.distance(pos) <= TAP_SLOP_PX {
        return;
    }
    let fingers = state.fingers.len();
    let (Some(before), Some(after)) = (before, state.steering_pair()) else {
        return;
    };
    let Some(session) = state.session.as_mut() else {
        return;
    };
    session.moved = true;
    let nav = session.plan.navigation(fingers);
    if session.palm || before == after || !nav.any() {
        return;
    }
    let (scale, turn) = spread_change(before, after);
    if nav.rotate {
        session.add_turn(turn);
    }
    let rotation = session.shown_rotation(context.modifiers);
    step(View::read(context.sync), before, after, nav, scale, rotation).write(context.sync);
}

fn release(
    state: &mut TouchState,
    sequence: &gdk::EventSequence,
    cancelled: bool,
    pump: &super::RenderPump,
) -> Option<(TouchAction, Point)> {
    state.fingers.retain(|f| &f.sequence != sequence);
    if cancelled && let Some(session) = state.session.as_mut() {
        session.moved = true;
    }
    if !state.fingers.is_empty() {
        return None;
    }
    let session = state.session.take()?;
    pump.disarm();
    session.tapped().map(|action| (action, session.tap_point()))
}

// Zoom factor and turn (radians) from how the two fingers' spread changed.
fn spread_change(before: (Point, Point), after: (Point, Point)) -> (f32, f32) {
    let span_before = Point::new(before.1.x - before.0.x, before.1.y - before.0.y);
    let span_after = Point::new(after.1.x - after.0.x, after.1.y - after.0.y);
    let len_before = span_before.x.hypot(span_before.y);
    let len_after = span_after.x.hypot(span_after.y);
    if len_before > MIN_SPAN_PX && len_after > MIN_SPAN_PX {
        (
            len_after / len_before,
            wrap_pi(span_after.y.atan2(span_after.x) - span_before.y.atan2(span_before.x)),
        )
    } else {
        (1.0, 0.0)
    }
}

fn step(
    view: View,
    before: (Point, Point),
    after: (Point, Point),
    nav: Navigation,
    scale: f32,
    rotation: f32,
) -> View {
    let after = if nav.pan {
        after
    } else {
        let mid_before = before.0.lerp(before.1, 0.5);
        let mid_after = after.0.lerp(after.1, 0.5);
        let shift = Point::new(mid_before.x - mid_after.x, mid_before.y - mid_after.y);
        (
            Point::new(after.0.x + shift.x, after.0.y + shift.y),
            Point::new(after.1.x + shift.x, after.1.y + shift.y),
        )
    };
    let scale = if nav.zoom { scale } else { 1.0 };
    let rotation = if nav.rotate { rotation } else { view.rotation };
    navigate(view, before, after, scale, rotation)
}

// Moves the view so the canvas point under the fingers' midpoint follows it,
// at the new scale and rotation.
fn navigate(
    view: View,
    before: (Point, Point),
    after: (Point, Point),
    scale: f32,
    rotation: f32,
) -> View {
    let mid_before = before.0.lerp(before.1, 0.5);
    let mid_after = after.0.lerp(after.1, 0.5);
    let anchor = widget_to_canvas_xf(mid_before.x, mid_before.y, view.pan, view.zoom, view.rotation);
    let zoom = (view.zoom * scale).clamp(MIN_ZOOM, MAX_ZOOM);
    let (sin, cos) = rotation.sin_cos();
    let rotated = Point::new(
        cos.mul_add(anchor.x, -sin * anchor.y),
        sin.mul_add(anchor.x, cos * anchor.y),
    );
    View {
        pan: Point::new(zoom.mul_add(-rotated.x, mid_after.x), zoom.mul_add(-rotated.y, mid_after.y)),
        zoom,
        rotation,
    }
}


#[cfg(test)]
mod tests {
    use std::f32::consts::FRAC_PI_4;
    use std::time::Instant;

    use relm4::gtk::gdk::ModifierType;

    use std::time::Duration;

    use super::{
        Navigation, Point, Session, Snap, TAP_MAX_DURATION, TAP_SLOP_PX, View, is_stray_dab, navigate,
        spread_change, step, widget_to_canvas_xf,
    };
    use crate::input::{InputSettings, TouchAction};

    fn session(rotation: f32) -> Session {
        Session {
            began: Instant::now(),
            max_fingers: 2,
            moved: true,
            palm: false,
            pending_turn: 0.0,
            rotating: false,
            free_rotation: rotation,
            snap: Some(Snap { mask: Some(ModifierType::CONTROL_MASK), step: FRAC_PI_4 }),
            plan: InputSettings::default().touchscreen.plan(),
            landed_sum: Point::ZERO,
            landed_count: 0,
        }
    }

    #[test]
    fn rotation_waits_for_the_threshold() {
        let mut s = session(0.3);
        s.add_turn(0.1);
        assert!((s.shown_rotation(ModifierType::empty()) - 0.3).abs() < 1e-6);
        s.add_turn(0.1);
        assert!(s.rotating);
        assert!((s.shown_rotation(ModifierType::empty()) - 0.3).abs() < 1e-6);
        s.add_turn(0.1);
        assert!((s.shown_rotation(ModifierType::empty()) - 0.4).abs() < 1e-6);
    }

    #[test]
    fn rotation_sticks_to_nearby_steps() {
        let mut s = session(FRAC_PI_4 - 0.05);
        s.rotating = true;
        assert!((s.shown_rotation(ModifierType::empty()) - FRAC_PI_4).abs() < 1e-6);
        s.add_turn(-0.2);
        let free = FRAC_PI_4 - 0.25;
        assert!((s.shown_rotation(ModifierType::empty()) - free).abs() < 1e-6);
        assert!((s.shown_rotation(ModifierType::CONTROL_MASK) - FRAC_PI_4).abs() < 1e-6);
    }

    #[test]
    fn taps_follow_the_bindings_and_land_between_the_fingers() {
        let mut s = session(0.0);
        s.moved = false;
        s.note_landing(Point::new(100.0, 200.0));
        s.note_landing(Point::new(200.0, 300.0));
        assert_eq!(s.tapped(), Some(TouchAction::Undo));
        assert_eq!(s.tap_point(), Point::new(150.0, 250.0));
        s.max_fingers = 3;
        s.note_landing(Point::new(0.0, 100.0));
        assert_eq!(s.tapped(), Some(TouchAction::QuickMenu));
        assert_eq!(s.tap_point(), Point::new(100.0, 200.0));
        s.note_landing(Point::new(100.0, 200.0));
        assert_eq!(s.tap_point(), Point::new(100.0, 200.0), "a finger landing twice still averages");
        s.max_fingers = 4;
        assert_eq!(s.tapped(), None);
        s.max_fingers = 2;
        s.palm = true;
        assert_eq!(s.tapped(), None);
        s.palm = false;
        s.moved = true;
        assert_eq!(s.tapped(), None);
    }

    #[test]
    fn only_a_quick_still_first_finger_is_a_stray_dab() {
        let start = Point::new(50.0, 50.0);
        let near = Point::new(50.0 + TAP_SLOP_PX - 1.0, 50.0);
        let far = Point::new(50.0 + TAP_SLOP_PX + 1.0, 50.0);
        let late = TAP_MAX_DURATION + Duration::from_millis(1);
        assert!(is_stray_dab(Duration::ZERO, start, start));
        assert!(is_stray_dab(TAP_MAX_DURATION, start, near));
        assert!(!is_stray_dab(Duration::ZERO, start, far));
        assert!(!is_stray_dab(late, start, start));
    }

    fn canvas_under(view: View, p: Point) -> Point {
        widget_to_canvas_xf(p.x, p.y, view.pan, view.zoom, view.rotation)
    }

    fn free_navigate(view: View, before: (Point, Point), after: (Point, Point)) -> View {
        let (scale, turn) = spread_change(before, after);
        navigate(view, before, after, scale, view.rotation + turn)
    }

    fn assert_close(a: Point, b: Point) {
        assert!(a.distance(b) < 1e-3, "{a:?} != {b:?}");
    }

    #[test]
    fn one_finger_pans() {
        let view = View { pan: Point::new(10.0, 20.0), zoom: 2.0, rotation: 0.3 };
        let finger = Point::new(100.0, 100.0);
        let moved = Point::new(130.0, 90.0);
        let after = free_navigate(view, (finger, finger), (moved, moved));
        assert_close(after.pan, Point::new(40.0, 10.0));
        assert!((after.zoom - 2.0).abs() < 1e-6);
        assert!((after.rotation - 0.3).abs() < 1e-6);
    }

    #[test]
    fn two_fingers_keep_their_canvas_points() {
        let view = View { pan: Point::new(-50.0, 30.0), zoom: 1.5, rotation: 0.2 };
        let before = (Point::new(100.0, 200.0), Point::new(300.0, 200.0));
        let after = (Point::new(120.0, 150.0), Point::new(220.0, 350.0));
        let first = canvas_under(view, before.0);
        let second = canvas_under(view, before.1);
        let moved = free_navigate(view, before, after);
        assert_close(canvas_under(moved, after.0), first);
        assert_close(canvas_under(moved, after.1), second);
    }

    #[test]
    fn snapped_rotation_keeps_the_midpoint() {
        let view = View { pan: Point::new(40.0, -10.0), zoom: 0.8, rotation: 0.1 };
        let before = (Point::new(100.0, 100.0), Point::new(200.0, 100.0));
        let after = (Point::new(110.0, 90.0), Point::new(190.0, 160.0));
        let middle = canvas_under(view, before.0.lerp(before.1, 0.5));
        let snapped = std::f32::consts::FRAC_PI_4;
        let moved = navigate(view, before, after, 1.0, snapped);
        assert!((moved.rotation - snapped).abs() < 1e-6);
        assert_close(canvas_under(moved, after.0.lerp(after.1, 0.5)), middle);
    }

    #[test]
    fn every_navigation_mix_moves_only_what_it_binds() {
        let view = View { pan: Point::new(40.0, -10.0), zoom: 0.8, rotation: 0.1 };
        let before = (Point::new(100.0, 100.0), Point::new(200.0, 100.0));
        let after = (Point::new(140.0, 60.0), Point::new(260.0, 190.0));
        let (scale, turn) = spread_change(before, after);
        let middle_before = before.0.lerp(before.1, 0.5);
        let canvas_middle = canvas_under(view, middle_before);
        for bits in 0..8u8 {
            let nav = Navigation { pan: bits & 1 != 0, zoom: bits & 2 != 0, rotate: bits & 4 != 0 };
            let moved = step(view, before, after, nav, scale, view.rotation + turn);
            let expected_zoom = if nav.zoom { view.zoom * scale } else { view.zoom };
            let expected_rotation = if nav.rotate { view.rotation + turn } else { view.rotation };
            assert!((moved.zoom - expected_zoom).abs() < 1e-5, "{nav:?}");
            assert!((moved.rotation - expected_rotation).abs() < 1e-6, "{nav:?}");
            let pinned_at = if nav.pan { after.0.lerp(after.1, 0.5) } else { middle_before };
            assert_close(canvas_under(moved, pinned_at), canvas_middle);
        }
    }
}
