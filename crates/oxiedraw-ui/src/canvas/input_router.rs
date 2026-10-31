//! First look at every canvas press: pen buttons, drag button filters and the toggle pan.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_utils::geometry::Point;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use super::{NavDrag, ViewSync, Viewport};
use crate::input::{self, InputSettings, PanMode, PointerAction};

struct TogglePan {
    area: gtk::Picture,
    nav: Rc<Cell<NavDrag>>,
    pump: super::RenderPump,
    sync: ViewSync,
    last: Cell<Option<(f64, f64)>>,
    swallowed_release: Cell<Option<u32>>,
    watch: RefCell<Option<(gtk::Widget, gtk::EventControllerLegacy)>>,
    ending_release: Cell<Option<u32>>,
}

impl TogglePan {
    fn is_on(&self) -> bool {
        self.last.get().is_some()
    }

    fn start(self: &Rc<Self>, at: (f64, f64)) {
        self.last.set(Some(at));
        self.nav.set(NavDrag::PanToggle);
        self.pump.arm();
        self.area.set_cursor_from_name(Some("grabbing"));
        self.watch_window();
    }

    fn stop(&self) {
        if self.last.take().is_none() {
            return;
        }
        self.nav.set(NavDrag::None);
        self.pump.disarm();
        self.area.set_cursor_from_name(None);
    }

    // Fix: a press anywhere in the window ends the pan, not just one reaching the canvas.
    fn watch_window(self: &Rc<Self>) {
        self.unwatch();
        let Some(root) = self.area.root() else {
            return;
        };
        let root: gtk::Widget = root.upcast();
        let legacy = gtk::EventControllerLegacy::new();
        legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
        let toggle = Rc::downgrade(self);
        let window = root.downgrade();
        legacy.connect_event(move |_, event| match (toggle.upgrade(), window.upgrade()) {
            (Some(toggle), Some(window)) => toggle.on_window_event(&window, event),
            _ => glib::Propagation::Proceed,
        });
        root.add_controller(legacy.clone());
        self.watch.replace(Some((root, legacy)));
    }

    // Removed on idle: GTK may still be walking the controller list.
    fn unwatch(&self) {
        self.ending_release.set(None);
        if let Some((root, legacy)) = self.watch.take() {
            glib::idle_add_local_once(move || root.remove_controller(&legacy));
        }
    }

    fn on_window_event(&self, window: &gtk::Widget, event: &gdk::Event) -> glib::Propagation {
        let button = event.downcast_ref::<gdk::ButtonEvent>().map(gdk::ButtonEvent::button);
        match event.event_type() {
            gdk::EventType::ButtonPress | gdk::EventType::TouchBegin if self.is_on() => {
                self.stop();
                let swallow = self.over_canvas(window, event);
                if swallow && button.is_some() {
                    self.ending_release.set(button);
                } else {
                    self.unwatch();
                }
                if swallow { glib::Propagation::Stop } else { glib::Propagation::Proceed }
            }
            gdk::EventType::ButtonRelease if button.is_some() && self.ending_release.get() == button => {
                self.unwatch();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    }

    fn over_canvas(&self, window: &gtk::Widget, event: &gdk::Event) -> bool {
        let Some((x, y)) = crate::brush_popup::event_position_in(window, event) else {
            return false;
        };
        window
            .pick(x, y, gtk::PickFlags::DEFAULT)
            .is_some_and(|hit| hit == *self.area.upcast_ref::<gtk::Widget>())
    }

    fn follow(&self, at: (f64, f64)) {
        let Some(last) = self.last.replace(Some(at)) else {
            return;
        };
        #[allow(clippy::cast_possible_truncation)]
        let delta = Point::new((at.0 - last.0) as f32, (at.1 - last.1) as f32);
        if delta == Point::ZERO {
            return;
        }
        let pan = self.sync.pan.get();
        self.sync.pan.set(Point::new(pan.x + delta.x, pan.y + delta.y));
        self.sync.commit();
    }
}


pub(super) fn install(
    area: &gtk::Picture,
    viewport: &Viewport,
    primary_drag: &gtk::GestureDrag,
    pan_drag: &gtk::GestureDrag,
) {
    let toggle = Rc::new(TogglePan {
        area: area.clone(),
        nav: Rc::clone(&viewport.nav),
        pump: viewport.render_pump(),
        sync: viewport.view_sync(),
        last: Cell::new(None),
        swallowed_release: Cell::new(None),
        watch: RefCell::new(None),
        ending_release: Cell::new(None),
    });
    {
        let toggle = Rc::clone(&toggle);
        area.connect_unmap(move |_| {
            toggle.stop();
            toggle.unwatch();
        });
    }
    let legacy = gtk::EventControllerLegacy::new();
    legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
    let primary_drag = primary_drag.downgrade();
    let pan_drag = pan_drag.downgrade();
    let filters = RefCell::new(None::<(u32, u32)>);
    legacy.connect_event(move |_, event| match event.event_type() {
        gdk::EventType::ProximityIn => {
            let _ = input::note_pen_event(event);
            glib::Propagation::Proceed
        }
        gdk::EventType::ButtonPress => {
            let _ = input::note_pen_event(event);
            let wanted = button_filters();
            if filters.replace(Some(wanted)) != Some(wanted) {
                if let Some(drag) = primary_drag.upgrade() {
                    drag.set_button(wanted.0);
                }
                if let Some(drag) = pan_drag.upgrade() {
                    drag.set_button(wanted.1);
                }
            }
            let stroking = primary_drag.upgrade().is_some_and(|drag| drag.is_active());
            on_press(&toggle, event, stroking || toggle.nav.get() != NavDrag::None)
        }
        gdk::EventType::ButtonRelease => {
            let button = event.downcast_ref::<gdk::ButtonEvent>().map(gdk::ButtonEvent::button);
            if button.is_some() && toggle.swallowed_release.get() == button {
                toggle.swallowed_release.set(None);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
        gdk::EventType::MotionNotify => {
            if toggle.is_on()
                && let Some(at) = event.position()
            {
                toggle.follow(at);
            }
            glib::Propagation::Proceed
        }
        _ => glib::Propagation::Proceed,
    });
    area.add_controller(legacy);
}

fn on_press(toggle: &Rc<TogglePan>, event: &gdk::Event, busy: bool) -> glib::Propagation {
    let button = event.downcast_ref::<gdk::ButtonEvent>().map(gdk::ButtonEvent::button);
    if toggle.is_on() {
        toggle.stop();
        toggle.swallowed_release.set(button);
        return glib::Propagation::Stop;
    }
    // Fix: a toggle started mid-stroke moved the view under the pen.
    if busy || input::resolve_event(event) != Some(PointerAction::Pan(PanMode::Toggle)) {
        return glib::Propagation::Proceed;
    }
    let Some(at) = event.position() else {
        return glib::Propagation::Proceed;
    };
    toggle.start(at);
    toggle.swallowed_release.set(button);
    glib::Propagation::Stop
}

pub(super) fn button_filters() -> (u32, u32) {
    input::read(|s: &InputSettings| (s.draw_button_filter(), s.hold_pan_button_filter()))
}
