use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use oxiedraw_core::renderer::MAX_LAYERS;
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

/// Cheaply clonable handle for posting toast notifications.
///
/// Transient info/error toasts are shown in a single persistent pill that lives
/// as an overlay child of the canvas's `gtk::Overlay`. It is shown and hidden by
/// animating its opacity (a CSS class), never by mapping/unmapping a widget.
/// That matters because mapping a widget inside the canvas's surface mid-stroke
/// resets a drawing tablet's implicit grab and aborts the stroke (a mouse's
/// button grab survives it, so it looked tablet-only). Keeping the pill in the
/// same surface - rather than a separate popover surface - also avoids forcing
/// full compositor composition every frame, which made drawing lag.
///
/// Persistent (`pending`) and action toasts still use the `adw::ToastOverlay`:
/// they need a button or a live handle and only fire on deliberate, non-drawing
/// actions (save/export), never mid-stroke.
#[derive(Clone)]
pub(crate) struct Toaster(Rc<RefCell<Inner>>);

struct Inner {
    /// Native overlay, used only for `pending` / `action` toasts.
    overlay: Option<adw::ToastOverlay>,
    /// Persistent transient-toast pill and its label (opacity-toggled).
    pill: Option<gtk::Box>,
    label: Option<gtk::Label>,
    /// Auto-dismiss timer for the message currently on screen.
    timer: Option<glib::SourceId>,
    /// True while the pill is revealed.
    showing: bool,
    /// Flips each pulse so a fresh CSS class restarts the pop keyframe (re-adding
    /// the same class in one frame would not re-trigger the animation).
    pop_toggle: bool,
}

impl Toaster {
    /// Create an unbound toaster. Call [`bind`] once the widget tree exists.
    pub(crate) fn new() -> Self {
        Self(Rc::new(RefCell::new(Inner {
            overlay: None,
            pill: None,
            label: None,
            timer: None,
            showing: false,
            pop_toggle: false,
        })))
    }

    /// Wire the toaster to the native overlay (for action/pending toasts) and
    /// build the transient-toast pill as an overlay child of `host`.
    pub(crate) fn bind(&self, overlay: adw::ToastOverlay, host: &gtk::Overlay) {
        ensure_css();

        let label = gtk::Label::new(None);
        label.set_wrap(true);
        label.set_justify(gtk::Justification::Center);

        let pill = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        pill.append(&label);
        pill.add_css_class("oxie-toast");
        pill.set_halign(gtk::Align::Center);
        pill.set_valign(gtk::Align::End);
        // Never a pointer/stylus/focus target: it must not steal events from the
        // canvas, and being a persistent (always-mapped) widget it never breaks
        // the active gesture.
        pill.set_can_target(false);
        pill.set_can_focus(false);
        host.add_overlay(&pill);

        {
            let mut inner = self.0.borrow_mut();
            inner.overlay = Some(overlay);
            inner.pill = Some(pill);
            inner.label = Some(label);
        }
    }

    /// Show a 3-second informational toast.
    pub(crate) fn info(&self, msg: &str) {
        self.enqueue(msg, 3);
    }

    /// Show a 5-second error toast.
    pub(crate) fn error(&self, msg: &str) {
        self.enqueue(msg, 5);
    }

    /// Show a persistent toast (no auto-dismiss) on the native overlay. Returns
    /// the toast so the caller can dismiss it when the operation finishes. Used
    /// for long operations (save/export), never mid-stroke. Returns `None` if
    /// the overlay isn't bound yet.
    pub(crate) fn pending(&self, msg: &str) -> Option<adw::Toast> {
        let inner = self.0.borrow();
        let overlay = inner.overlay.as_ref()?;
        let t = adw::Toast::new(msg);
        t.set_timeout(0);
        overlay.add_toast(t.clone());
        Some(t)
    }

    /// Show a toast carrying an action button on the native overlay. `on_click`
    /// fires when the user presses the button. 5-second timeout.
    pub(crate) fn action(&self, msg: &str, button_label: &str, on_click: impl Fn() + 'static) {
        let inner = self.0.borrow();
        let Some(overlay) = inner.overlay.as_ref() else {
            return;
        };
        let t = adw::Toast::new(msg);
        t.set_timeout(5);
        t.set_button_label(Some(button_label));
        t.connect_button_clicked(move |_| on_click());
        overlay.add_toast(t);
    }

    /// Show the standard "layer limit reached" toast. Kept here so every
    /// add-layer site reports the same wording and the same cap.
    pub(crate) fn layer_limit_reached(&self) {
        self.error(&format!(
            "Layer limit reached ({MAX_LAYERS} layers maximum per canvas)"
        ));
    }

    /// Show `msg` for `duration` seconds. If the pill is already on screen (a
    /// different event), update the text and replay the scale pop; otherwise
    /// play the entrance. Either way the auto-dismiss timer is reset.
    fn enqueue(&self, msg: &str, duration: u32) {
        let (pill, label, was_showing) = {
            let mut inner = self.0.borrow_mut();
            let (Some(pill), Some(label)) = (inner.pill.clone(), inner.label.clone()) else {
                return;
            };
            if let Some(id) = inner.timer.take() {
                id.remove();
            }
            let was = inner.showing;
            inner.showing = true;
            (pill, label, was)
        };
        label.set_text(msg);
        if was_showing {
            self.pulse(&pill);
        } else {
            pill.add_css_class("revealed");
        }
        self.arm_dismiss(duration);
    }

    /// Replay the scale pop on the already-revealed pill. Alternating two
    /// classes guarantees a fresh class application each time, which restarts
    /// the keyframe (re-adding the same class in one frame would not).
    fn pulse(&self, pill: &gtk::Box) {
        let toggle = {
            let mut inner = self.0.borrow_mut();
            inner.pop_toggle = !inner.pop_toggle;
            inner.pop_toggle
        };
        if toggle {
            pill.remove_css_class("pop-b");
            pill.add_css_class("pop-a");
        } else {
            pill.remove_css_class("pop-a");
            pill.add_css_class("pop-b");
        }
    }

    /// Arm the timer that fades the pill out after `duration` seconds.
    fn arm_dismiss(&self, duration: u32) {
        let this = self.clone();
        let id = glib::timeout_add_local_once(Duration::from_secs(u64::from(duration)), move || {
            let pill = {
                let mut inner = this.0.borrow_mut();
                inner.timer = None;
                inner.showing = false;
                inner.pill.clone()
            };
            if let Some(pill) = pill {
                pill.remove_css_class("revealed");
                pill.remove_css_class("pop-a");
                pill.remove_css_class("pop-b");
            }
        });
        self.0.borrow_mut().timer = Some(id);
    }
}

/// Install the toast pill stylesheet once per display. Theme-adaptive via
/// libadwaita named colours so it matches a native notification in light/dark.
fn ensure_css() {
    thread_local! {
        static LOADED: Cell<bool> = const { Cell::new(false) };
    }
    if LOADED.with(|l| l.replace(true)) {
        return;
    }
    let css = "
/* Panel (sidebar) colour, theme-adaptive, with a rounded-card shape. */
.oxie-toast {
    margin: 12px;
    border-radius: 12px;
    background-color: @sidebar_bg_color;
    color: @sidebar_fg_color;
    box-shadow: 0 1px 2px alpha(black, 0.12), 0 2px 8px alpha(black, 0.16);
    opacity: 0;
    transform: scale(0.9);
    transition: opacity 200ms ease-in-out, transform 200ms ease-in-out;
}
.oxie-toast.revealed {
    opacity: 1;
    transform: scale(1);
}
.oxie-toast > label {
    margin: 10px 16px;
}
@keyframes oxie-toast-pop {
    0% { transform: scale(1); }
    45% { transform: scale(0.9); }
    100% { transform: scale(1); }
}
.oxie-toast.pop-a, .oxie-toast.pop-b {
    animation: oxie-toast-pop 220ms ease-in-out;
}
";
    let provider = gtk::CssProvider::new();
    provider.load_from_string(css);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
