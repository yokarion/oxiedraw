use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use oxiedraw_core::renderer::MAX_LAYERS;
use relm4::gtk::glib;

use crate::canvas_paintable::CanvasPaintable;

/// Distance (px) the pill slides down as it fades in (and back up on the way
/// out). The paintable draws the pill top-center and subtracts this offset.
const SLIDE_PX: f32 = 10.0;
/// Fade-in / fade-out durations in milliseconds.
const IN_MS: f32 = 180.0;
const OUT_MS: f32 = 200.0;
/// Animation tick interval. Only runs during the ~200ms fade phases - the
/// multi-second hold in between schedules a single one-shot, so an idle toast
/// costs no per-frame work.
const TICK_MS: u64 = 16;

/// Cheaply clonable handle for posting toast notifications.
///
/// Every toast the main window raises - transient and persistent alike - is
/// drawn as a pill *inside the active canvas's paintable*
/// (`CanvasPaintable::set_toast_*`), not as a GTK widget. A widget
/// mapping/unmapping inside the canvas surface mid-stroke resets a drawing
/// tablet's implicit grab and aborts the stroke; a pill painted into the canvas
/// content never touches the widget tree, so it can't. The slide/fade animates
/// through the paintable's cheap invalidate path (the same one the marching ants
/// use) - no canvas re-composite, no stutter.
///
/// The `adw::ToastOverlay` survives only as a last-resort fallback for messages
/// raised before any canvas exists (see [`Toaster::fallback`]).
#[derive(Clone)]
pub(crate) struct Toaster(Rc<RefCell<Inner>>);

struct Inner {
    /// Native overlay, used only when there is no canvas to draw into.
    overlay: Option<adw::ToastOverlay>,
    /// The active document's paintable; toasts are drawn into it.
    /// Re-pointed on every tab switch (see `set_target`).
    target: Option<CanvasPaintable>,
    /// Frame ticker for the fade-in / fade-out phases.
    tick: Option<glib::SourceId>,
    /// One-shot timer for the on-screen hold between fade-in and fade-out.
    hold: Option<glib::SourceId>,
    /// True once the fade-in finished (pill fully on screen).
    visible: bool,
    /// True from the moment a toast is posted until its fade-out finishes.
    /// Distinct from `visible`, which only covers the fully-faded-in stretch.
    active: bool,
    /// Current pill opacity, so a dismissal mid fade-in can fade out from where
    /// the animation actually is instead of snapping to full.
    alpha: f32,
    /// Bumped by every `show`. A [`PendingToast`] only dismisses the toast it
    /// posted, so a later message can't be cut short by a stale handle.
    generation: u64,
}

/// Handle to a toast with no auto-dismiss, returned by [`Toaster::pending`].
/// Dropping it does nothing - the toast stays up until [`PendingToast::dismiss`]
/// is called or another message replaces it.
pub(crate) struct PendingToast {
    toaster: Toaster,
    generation: u64,
}

impl PendingToast {
    /// Fade the toast out, unless a newer message has already taken the pill.
    /// Safe to call more than once.
    pub(crate) fn dismiss(&self) {
        self.toaster.dismiss_generation(self.generation);
    }
}

impl Toaster {
    /// Create an unbound toaster. Call [`bind`] / [`set_target`] once the widget
    /// tree and a document exist.
    pub(crate) fn new() -> Self {
        Self(Rc::new(RefCell::new(Inner {
            overlay: None,
            target: None,
            tick: None,
            hold: None,
            visible: false,
            active: false,
            alpha: 0.0,
            generation: 0,
        })))
    }

    /// Wire the native overlay used for the no-canvas fallback.
    pub(crate) fn bind(&self, overlay: adw::ToastOverlay) {
        self.0.borrow_mut().overlay = Some(overlay);
    }

    /// Point transient toasts at `paintable` (the now-active document). Clears
    /// any in-flight toast on the previous canvas so it doesn't linger there.
    pub(crate) fn set_target(&self, paintable: CanvasPaintable) {
        self.cancel_timers();
        let mut inner = self.0.borrow_mut();
        if let Some(old) = inner.target.take() {
            old.set_toast_message(None);
            old.set_toast_anim(0.0, 0.0);
        }
        inner.visible = false;
        inner.active = false;
        inner.alpha = 0.0;
        inner.target = Some(paintable);
    }

    /// Show a 3-second informational toast.
    pub(crate) fn info(&self, msg: &str) {
        self.show(msg, Some(3));
    }

    /// Show a 5-second error toast.
    pub(crate) fn error(&self, msg: &str) {
        self.show(msg, Some(5));
    }

    /// Show a toast that stays up until the returned handle is dismissed. For
    /// long operations (saving, decoding a pasted image) where the end time
    /// isn't known up front.
    pub(crate) fn pending(&self, msg: &str) -> PendingToast {
        self.show(msg, None);
        PendingToast {
            toaster: self.clone(),
            generation: self.0.borrow().generation,
        }
    }

    /// Show the standard "layer limit reached" toast. Kept here so every
    /// add-layer site reports the same wording and the same cap.
    pub(crate) fn layer_limit_reached(&self) {
        self.error(&format!(
            "Layer limit reached ({MAX_LAYERS} layers maximum per canvas)"
        ));
    }

    /// Show `msg` in the active canvas's pill, auto-dismissing after
    /// `hold_secs` (or staying up indefinitely when `None`). If the pill is
    /// already on screen, swap the text in place and just restart the hold (no
    /// re-slide); otherwise play the fade/slide entrance.
    fn show(&self, msg: &str, hold_secs: Option<u64>) {
        let target = self.0.borrow().target.clone();
        let Some(target) = target else {
            // No active canvas yet (e.g. an error during startup): fall back to a
            // native toast so the message isn't silently lost.
            self.fallback(msg, hold_secs);
            return;
        };
        target.set_toast_message(Some(msg));
        // Stop any running fade so a re-show (including one arriving mid fade-out)
        // doesn't fight the ticker. `visible` stays true through the fade-out, so
        // a toast caught on its way out just snaps back to full and re-holds.
        self.cancel_tick();
        let was_visible = {
            let mut inner = self.0.borrow_mut();
            if let Some(id) = inner.hold.take() {
                id.remove();
            }
            inner.generation += 1;
            inner.active = true;
            inner.visible
        };
        if was_visible {
            target.set_toast_anim(1.0, 0.0);
            self.0.borrow_mut().alpha = 1.0;
            if let Some(secs) = hold_secs {
                self.start_hold(secs);
            }
        } else {
            self.start_fade_in(hold_secs);
        }
    }

    /// Drive the fade-in over `IN_MS`, then transition to the hold phase (or
    /// simply stay put when there's no hold). Callers (`show`) cancel any
    /// running tick first.
    fn start_fade_in(&self, hold_secs: Option<u64>) {
        let start = Instant::now();
        let this = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(TICK_MS), move || {
            let Some(target) = this.0.borrow().target.clone() else {
                return glib::ControlFlow::Break;
            };
            let p = (start.elapsed().as_secs_f32() * 1000.0 / IN_MS).min(1.0);
            let eased = 1.0 - (1.0 - p).powi(3);
            target.set_toast_anim(eased, (1.0 - eased) * SLIDE_PX);
            this.0.borrow_mut().alpha = eased;
            if p >= 1.0 {
                {
                    let mut inner = this.0.borrow_mut();
                    inner.tick = None;
                    inner.visible = true;
                }
                if let Some(secs) = hold_secs {
                    this.start_hold(secs);
                }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        self.0.borrow_mut().tick = Some(id);
    }

    /// Hold the pill fully visible for `hold_secs`, then fade out. No per-frame
    /// work during the hold - a single one-shot timer.
    fn start_hold(&self, hold_secs: u64) {
        let this = self.clone();
        let id = glib::timeout_add_local_once(Duration::from_secs(hold_secs), move || {
            this.0.borrow_mut().hold = None;
            this.start_fade_out();
        });
        self.0.borrow_mut().hold = Some(id);
    }

    /// Drive the fade-out over `OUT_MS`, then clear the pill. Starts from the
    /// pill's current opacity so dismissing one mid fade-in doesn't pop it to
    /// full brightness first.
    fn start_fade_out(&self) {
        self.cancel_tick();
        let start = Instant::now();
        let from = self.0.borrow().alpha;
        let this = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(TICK_MS), move || {
            let Some(target) = this.0.borrow().target.clone() else {
                return glib::ControlFlow::Break;
            };
            let p = (start.elapsed().as_secs_f32() * 1000.0 / OUT_MS).min(1.0);
            let eased = p * p;
            let alpha = from * (1.0 - eased);
            target.set_toast_anim(alpha, eased * SLIDE_PX);
            this.0.borrow_mut().alpha = alpha;
            if p >= 1.0 {
                {
                    let mut inner = this.0.borrow_mut();
                    inner.tick = None;
                    inner.visible = false;
                    inner.active = false;
                    inner.alpha = 0.0;
                }
                target.set_toast_message(None);
                target.set_toast_anim(0.0, 0.0);
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        self.0.borrow_mut().tick = Some(id);
    }

    /// Fade out the toast posted at `generation`. No-op once a newer message has
    /// taken the pill or the toast is already gone.
    fn dismiss_generation(&self, generation: u64) {
        {
            let inner = self.0.borrow();
            if !inner.active || inner.generation != generation {
                return;
            }
        }
        self.cancel_timers();
        self.start_fade_out();
    }

    fn cancel_tick(&self) {
        if let Some(id) = self.0.borrow_mut().tick.take() {
            id.remove();
        }
    }

    fn cancel_timers(&self) {
        let mut inner = self.0.borrow_mut();
        if let Some(id) = inner.tick.take() {
            id.remove();
        }
        if let Some(id) = inner.hold.take() {
            id.remove();
        }
    }

    /// Last-resort native toast when there's no active canvas to draw into (an
    /// error raised before the first document exists). Never reachable while a
    /// stroke is in flight, so it can't break a tablet grab.
    fn fallback(&self, msg: &str, hold_secs: Option<u64>) {
        let inner = self.0.borrow();
        if let Some(overlay) = inner.overlay.as_ref() {
            let t = adw::Toast::new(msg);
            // A `PendingToast` handle can't reach a native toast, so give the
            // no-hold case a timeout anyway rather than let it stick forever.
            #[allow(clippy::cast_possible_truncation)]
            t.set_timeout(hold_secs.unwrap_or(10) as u32);
            overlay.add_toast(t);
        }
    }
}
