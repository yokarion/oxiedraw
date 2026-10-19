//! Touchscreen navigation: one finger pans, two fingers pan, zoom and rotate
//! together, and a quick two-finger tap undoes. Touches never draw.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use oxiedraw_utils::geometry::Point;
use oxiedraw_utils::math::wrap_pi;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use super::{MAX_ZOOM, MIN_ZOOM, ViewSync, Viewport, widget_to_canvas_xf};

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
}

impl Session {
    fn new(rotation: f32) -> Self {
        Self {
            began: Instant::now(),
            max_fingers: 0,
            moved: false,
            palm: false,
            pending_turn: 0.0,
            rotating: false,
            free_rotation: rotation,
            snap: None,
        }
    }

    fn is_two_finger_tap(&self) -> bool {
        self.max_fingers == 2
            && !self.moved
            && !self.palm
            && self.began.elapsed() <= TAP_MAX_DURATION
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

#[derive(Default)]
struct TouchState {
    fingers: Vec<Finger>,
    session: Option<Session>,
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
        if self.session.take().is_some() {
            pump.disarm();
        }
    }
}


pub(super) fn install_touch_nav(
    area: &gtk::Picture,
    viewport: &Viewport,
    primary_drag: &gtk::GestureDrag,
) {
    let legacy = gtk::EventControllerLegacy::new();
    // Capture phase, consuming every touch event, so a finger never reaches
    // the drawing gesture on the same widget.
    legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
    let state = Rc::new(RefCell::new(TouchState::default()));
    {
        // A tab switch can unmap the canvas between a finger's press and its lift.
        let state = Rc::clone(&state);
        let pump = viewport.render_pump();
        area.connect_unmap(move |_| state.borrow_mut().reset(&pump));
    }
    let sync = Rc::new(viewport.view_sync());
    let pump = viewport.render_pump();
    let animator = viewport.rotation_animator();
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
        let sequence = event.event_sequence();
        #[allow(clippy::cast_possible_truncation)]
        let pos = crate::brush_popup::event_position_in(&area_c, event)
            .map(|(x, y)| Point::new(x as f32, y as f32));
        // GTK's own gesture state, so a denied or lost pen drag can't leave
        // touch navigation switched off.
        let pen_down = primary_drag.upgrade().is_some_and(|g| g.is_active());
        let undo = {
            let mut state = state.borrow_mut();
            // The first finger of a fresh contact emulates the pointer; any
            // fingers still tracked then are stale.
            let fresh = kind == gdk::EventType::TouchBegin
                && event
                    .downcast_ref::<gdk::TouchEvent>()
                    .is_some_and(gdk::TouchEvent::is_emulating_pointer);
            if fresh {
                state.reset(&pump);
            }
            if pen_down && let Some(session) = state.session.as_mut() {
                session.palm = true;
            }
            match (kind, pos) {
                (gdk::EventType::TouchBegin, Some(pos)) => {
                    if state.session.is_none() {
                        animator.cancel();
                    }
                    press(&mut state, sequence, pos, &sync, &pump);
                    if let Some(session) = state.session.as_mut() {
                        session.palm |= pen_down;
                    }
                    false
                }
                (gdk::EventType::TouchUpdate, Some(pos)) => {
                    drag(&mut state, &sequence, pos, event.modifier_state(), &sync);
                    false
                }
                (gdk::EventType::TouchEnd | gdk::EventType::TouchCancel, _) => release(
                    &mut state,
                    &sequence,
                    kind == gdk::EventType::TouchCancel,
                    &pump,
                ),
                _ => false,
            }
        };
        if undo && let Err(e) = area_c.activate_action("app.undo", None) {
            tracing::warn!(%e, "two-finger tap: undo action missing");
        }
        glib::Propagation::Stop
    });
    area.add_controller(legacy);
}

fn press(
    state: &mut TouchState,
    sequence: gdk::EventSequence,
    pos: Point,
    sync: &ViewSync,
    pump: &super::RenderPump,
) {
    if state.session.is_none() {
        pump.arm();
        state.session = Some(Session::new(sync.rotation.get()));
    }
    state.fingers.push(Finger { sequence, start: pos, pos });
    let count = state.fingers.len();
    if let Some(session) = state.session.as_mut() {
        session.max_fingers = session.max_fingers.max(count);
    }
}

fn drag(
    state: &mut TouchState,
    sequence: &gdk::EventSequence,
    pos: Point,
    modifiers: gdk::ModifierType,
    sync: &ViewSync,
) {
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
    let (Some(before), Some(after)) = (before, state.steering_pair()) else {
        return;
    };
    let Some(session) = state.session.as_mut() else {
        return;
    };
    session.moved = true;
    if session.palm || before == after {
        return;
    }
    let (scale, turn) = spread_change(before, after);
    session.add_turn(turn);
    let rotation = session.shown_rotation(modifiers);
    navigate(View::read(sync), before, after, scale, rotation).write(sync);
}

// Returns whether the session that just ended was a two-finger tap.
fn release(
    state: &mut TouchState,
    sequence: &gdk::EventSequence,
    cancelled: bool,
    pump: &super::RenderPump,
) -> bool {
    state.fingers.retain(|f| &f.sequence != sequence);
    if cancelled && let Some(session) = state.session.as_mut() {
        session.moved = true;
    }
    if !state.fingers.is_empty() {
        return false;
    }
    let Some(session) = state.session.take() else {
        return false;
    };
    pump.disarm();
    session.is_two_finger_tap()
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

    use super::{Point, Session, Snap, View, navigate, spread_change, widget_to_canvas_xf};

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
}
