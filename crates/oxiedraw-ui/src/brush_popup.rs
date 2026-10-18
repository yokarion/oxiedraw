//! Krita-style quick popup for the Brush tool: the secondary button (right
//! click or the pen's barrel button) opens the colour wheel in a disc at the
//! pointer, with a brush size slider below it. Any press outside those, or the
//! secondary button again, closes it.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use oxiedraw_core::brush_engine::BrushEngine;
use oxiedraw_core::color::ColorState;
use oxiedraw_core::tools::{Tool, ToolState};
use relm4::RelmWidgetExt;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::panels::color_picker;
use crate::panels::tool_options::build_size_slider;

const DISC_DIAMETER: i32 = 184;
const WHEEL_SIZE: i32 = 160;
const SLIDER_WIDTH: i32 = 220;
const ANIMATION_MS: u64 = 120;
const OPEN_CLASS: &str = "open";
const SHRUNK_CLASS: &str = "shrunk";
const INSTANT_CLASS: &str = "instant";
const BACKDROP_CSS: &str = "background-color: alpha(@window_bg_color, 0.5); \
     border: 1px solid alpha(@borders, 0.8); border-radius: 9999px;";


// Returns a handle that closes the popup, for when the tool changes under it.
pub(crate) fn attach(
    overlay: &gtk::Overlay,
    picture: &gtk::Picture,
    tools: &ToolState,
    colors: &ColorState,
    brush_engine: &BrushEngine,
) -> Rc<dyn Fn()> {
    let size_slider = build_size_slider(brush_engine, SLIDER_WIDTH);
    size_slider.set_draw_value(false);
    size_slider.set_halign(gtk::Align::Center);
    let (popup, slider_box) = build_popup(colors, &size_slider);
    overlay.add_overlay(&popup);
    let state = PopupState {
        popup,
        slider_box,
        is_open: Rc::new(Cell::new(false)),
        generation: Rc::new(Cell::new(0)),
        buttons_held: Rc::new(Cell::new(0)),
        hide_pending: Rc::new(Cell::new(false)),
    };

    // A legacy controller because it never claims a press: a gesture holding
    // the pen's sequence cancels the stroke that follows.
    let events = gtk::EventControllerLegacy::new();
    events.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let state = state.clone();
        let tools = tools.clone();
        // Weak: both widgets own this closure through their controllers.
        let overlay = overlay.downgrade();
        let picture = picture.downgrade();
        events.connect_event(move |_, event| {
            let (Some(overlay), Some(picture)) = (overlay.upgrade(), picture.upgrade()) else {
                return glib::Propagation::Proceed;
            };
            match event.event_type() {
                gdk::EventType::ButtonPress => {
                    state.buttons_held.set(state.buttons_held.get() + 1);
                    on_press(&state, &tools, &overlay, &picture, event)
                }
                gdk::EventType::ButtonRelease => {
                    state.buttons_held.set(state.buttons_held.get().saturating_sub(1));
                    state.hide_if_pending();
                    glib::Propagation::Proceed
                }
                gdk::EventType::MotionNotify => {
                    on_motion(&state, &overlay, event);
                    glib::Propagation::Proceed
                }
                _ => glib::Propagation::Proceed,
            }
        });
    }
    overlay.add_controller(events);

    {
        let state = state.clone();
        overlay.connect_unmap(move |_| state.close(true));
    }

    Rc::new(move || state.close(true))
}

// The secondary button opens or closes the popup and never reaches the canvas;
// any other press outside closes it and goes through, so a stroke starts at once.
fn on_press(
    state: &PopupState,
    tools: &ToolState,
    overlay: &gtk::Overlay,
    picture: &gtk::Picture,
    event: &gdk::Event,
) -> glib::Propagation {
    let Some((x, y)) = event_position_in(overlay, event) else {
        return glib::Propagation::Proceed;
    };
    let is_secondary = event
        .downcast_ref::<gdk::ButtonEvent>()
        .is_some_and(|button| button.button() == gdk::BUTTON_SECONDARY);

    if state.is_open.get() {
        if is_secondary {
            state.close(true);
            return glib::Propagation::Stop;
        }
        if !state.contains(overlay, x, y) {
            state.close(false);
        }
        return glib::Propagation::Proceed;
    }

    // The pop-in scale would cancel a stroke already under way.
    let stroke_running = event
        .modifier_state()
        .contains(gdk::ModifierType::BUTTON1_MASK);
    let over_canvas = overlay
        .pick(x, y, gtk::PickFlags::DEFAULT)
        .is_some_and(|hit| hit == *picture.upcast_ref::<gtk::Widget>());
    if !is_secondary || stroke_running || tools.active.get() != Tool::Brush || !over_canvas {
        return glib::Propagation::Proceed;
    }
    state.open(|popup| place_centered(popup, overlay, x, y));
    glib::Propagation::Stop
}

// Only the disc and the slider take input: GTK picks the press target before
// any controller runs, so the gaps around them are switched off ahead of time.
fn on_motion(state: &PopupState, overlay: &gtk::Overlay, event: &gdk::Event) {
    let buttons = gdk::ModifierType::BUTTON1_MASK
        | gdk::ModifierType::BUTTON2_MASK
        | gdk::ModifierType::BUTTON3_MASK;
    if !event.modifier_state().intersects(buttons) {
        // Heals a release that never reached us, such as one outside the window.
        state.buttons_held.set(0);
        state.hide_if_pending();
    }
    if !state.is_open.get() || state.buttons_held.get() > 0 {
        return;
    }
    if let Some((x, y)) = event_position_in(overlay, event) {
        state.popup.set_can_target(state.contains(overlay, x, y));
    }
}


// `generation` lets a stale tick or timeout from an earlier open/close notice
// it has been superseded and do nothing.
#[derive(Clone)]
struct PopupState {
    popup: gtk::Box,
    slider_box: gtk::Box,
    is_open: Rc<Cell<bool>>,
    generation: Rc<Cell<u64>>,
    buttons_held: Rc<Cell<u32>>,
    hide_pending: Rc<Cell<bool>>,
}

impl PopupState {
    fn bump(&self) -> u64 {
        let next = self.generation.get().wrapping_add(1);
        self.generation.set(next);
        next
    }

    fn open(&self, place: impl FnOnce(&gtk::Box)) {
        let current = self.bump();
        self.is_open.set(true);
        self.hide_pending.set(false);
        // Shown first: a hidden widget measures as 0x0, which skews the centring.
        self.popup.set_visible(true);
        place(&self.popup);
        self.popup.set_can_target(true);
        self.popup.set_can_focus(true);
        self.popup.add_css_class(INSTANT_CLASS);
        self.popup.add_css_class(SHRUNK_CLASS);
        // A frame later, as GTK skips transitions on a node that was just hidden.
        let generation = Rc::clone(&self.generation);
        self.popup.add_tick_callback(move |popup, _| {
            if generation.get() == current {
                popup.remove_css_class(INSTANT_CLASS);
                popup.remove_css_class(SHRUNK_CLASS);
                popup.add_css_class(OPEN_CLASS);
            }
            glib::ControlFlow::Break
        });
    }

    // Relayout mid-stroke cancels a pen drag, so shrinking and hiding wait for
    // a moment when nothing can be drawing; opacity alone only redraws.
    fn close(&self, shrink: bool) {
        if !self.is_open.get() {
            return;
        }
        let current = self.bump();
        self.is_open.set(false);
        self.popup.remove_css_class(INSTANT_CLASS);
        self.popup.remove_css_class(OPEN_CLASS);
        if shrink {
            self.popup.add_css_class(SHRUNK_CLASS);
        }
        self.popup.set_can_target(false);
        self.popup.set_can_focus(false);
        self.release_focus();
        let state = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(ANIMATION_MS), move || {
            if state.generation.get() != current {
                return;
            }
            if state.buttons_held.get() == 0 {
                state.popup.set_visible(false);
            } else {
                state.hide_pending.set(true);
            }
        });
    }

    // Deferred to idle so the canvas handles the release before the relayout.
    fn hide_if_pending(&self) {
        if !self.hide_pending.get() || self.buttons_held.get() > 0 {
            return;
        }
        self.hide_pending.set(false);
        let state = self.clone();
        glib::idle_add_local_once(move || {
            if !state.is_open.get() && state.buttons_held.get() == 0 {
                state.popup.set_visible(false);
            }
        });
    }

    fn release_focus(&self) {
        let Some(root) = self.popup.root() else {
            return;
        };
        if root.focus().is_some_and(|focus| focus.is_ancestor(&self.popup)) {
            root.set_focus(None::<&gtk::Widget>);
        }
    }

    fn contains(&self, overlay: &gtk::Overlay, x: f64, y: f64) -> bool {
        let half = f64::from(DISC_DIAMETER) / 2.0;
        let center_x = f64::from(self.popup.margin_start()) + f64::from(self.popup.width()) / 2.0;
        let center_y = f64::from(self.popup.margin_top()) + half;
        if (x - center_x).hypot(y - center_y) <= half {
            return true;
        }
        #[allow(clippy::cast_possible_truncation)]
        let point = gtk::graphene::Point::new(x as f32, y as f32);
        self.slider_box
            .compute_bounds(overlay)
            .is_some_and(|bounds| bounds.contains_point(&point))
    }
}


fn load_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&format!(
            ".oxiedraw-brush-popup {{
                opacity: 0;
                transform: scale(1);
                transform-origin: 50% {origin}px;
                transition: opacity {ANIMATION_MS}ms ease-out, transform {ANIMATION_MS}ms ease-out;
            }}

            .oxiedraw-brush-popup.{OPEN_CLASS} {{
                opacity: 1;
            }}

            .oxiedraw-brush-popup.{SHRUNK_CLASS} {{
                transform: scale(0.9);
            }}

            .oxiedraw-brush-popup.{INSTANT_CLASS} {{
                transition: none;
            }}",
            origin = DISC_DIAMETER / 2,
        ));
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
}

fn build_popup(colors: &ColorState, size_slider: &gtk::Scale) -> (gtk::Box, gtk::Box) {
    let disc = gtk::Box::builder()
        .width_request(DISC_DIAMETER)
        .height_request(DISC_DIAMETER)
        .build();
    disc.inline_css(BACKDROP_CSS);

    let wheel = color_picker::build_wheel(colors.clone(), WHEEL_SIZE);
    wheel.set_valign(gtk::Align::Center);

    let wheel_disc = gtk::Overlay::builder()
        .child(&disc)
        .halign(gtk::Align::Center)
        .build();
    wheel_disc.add_overlay(&wheel);

    let slider_box = gtk::Box::builder().halign(gtk::Align::Center).build();
    slider_box.inline_css(&format!("{BACKDROP_CSS} padding: 4px 14px;"));
    slider_box.append(size_slider);

    let popup = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .visible(false)
        .css_classes(["oxiedraw-brush-popup"])
        .build();
    load_css();
    popup.append(&wheel_disc);
    popup.append(&slider_box);
    (popup, slider_box)
}

// Disc centred on the pointer, clamped so the slider below stays on screen too.
fn place_centered(popup: &gtk::Box, overlay: &gtk::Overlay, x: f64, y: f64) {
    let half = f64::from(DISC_DIAMETER) / 2.0;
    // measure() includes margins, which still hold the previous position.
    popup.set_margin_start(0);
    popup.set_margin_top(0);
    let (_, width, _, _) = popup.measure(gtk::Orientation::Horizontal, -1);
    let (_, height, _, _) = popup.measure(gtk::Orientation::Vertical, width);
    #[allow(clippy::cast_possible_truncation)]
    let clamp = |start: f64, size: i32, extent: i32| {
        (start.round() as i32).clamp(0, (extent - size).max(0))
    };
    let disc_left = f64::from(width - DISC_DIAMETER) / 2.0;
    popup.set_margin_start(clamp(x - half - disc_left, width, overlay.width()));
    popup.set_margin_top(clamp(y - half, height, overlay.height()));
}

// Legacy events carry surface coordinates; map them into the overlay.
fn event_position_in(overlay: &gtk::Overlay, event: &gdk::Event) -> Option<(f64, f64)> {
    let (surface_x, surface_y) = event.position()?;
    let native = overlay.native()?;
    let (offset_x, offset_y) = native.surface_transform();
    #[allow(clippy::cast_possible_truncation)]
    let point = gtk::graphene::Point::new((surface_x - offset_x) as f32, (surface_y - offset_y) as f32);
    let mapped = native.upcast_ref::<gtk::Widget>().compute_point(overlay, &point)?;
    Some((f64::from(mapped.x()), f64::from(mapped.y())))
}
