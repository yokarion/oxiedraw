//! Per-frame CPU stage timing for the performance overlay (F3).
//!
//! Spans nest: a scope records its own elapsed time minus whatever its children
//! took, so the stage totals sum to the wall time of the outermost span without
//! double counting. Everything lives in one thread-local - the app is
//! single-threaded, so an active span costs two clock reads and a push/pop, and
//! a span with profiling off costs a single bool read.
//!
//! # Example
//! ```
//! use oxiedraw_utils::frame_profile::{Stage, span};
//! let _guard = span(Stage::Composite);
//! ```

use std::cell::RefCell;
use std::time::Instant;

/// Number of instrumented stages (the idle residual is derived, not timed).
pub const STAGE_COUNT: usize = 7;

/// One instrumented slice of a frame. The order is the order work happens in,
/// which is also the bottom-up order of the stacked breakdown chart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// GTK input callbacks: event decode, view transform, guide snapping.
    Input,
    /// Brush engine dab generation plus the GPU stamp record and submit.
    Brush,
    /// Preview composite: recording and submitting the frame's draw passes.
    Composite,
    /// Blocked on a GPU fence (ring-slot reuse or the present sync).
    GpuWait,
    /// Building the dmabuf texture and handing it to GTK.
    Texture,
    /// The paintable snapshot: cairo overlays and GSK nodes.
    Snapshot,
    /// Main-loop timer callbacks and blocking disk I/O - thumbnail readbacks,
    /// autosave, settings loads. These fire between frames, so without a span
    /// they would masquerade as idle time.
    Timers,
}

impl Stage {
    pub const ALL: [Self; STAGE_COUNT] = [
        Self::Input,
        Self::Brush,
        Self::Composite,
        Self::GpuWait,
        Self::Texture,
        Self::Snapshot,
        Self::Timers,
    ];

    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Input => 0,
            Self::Brush => 1,
            Self::Composite => 2,
            Self::GpuWait => 3,
            Self::Texture => 4,
            Self::Snapshot => 5,
            Self::Timers => 6,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Input => "Input",
            Self::Brush => "Brush + stamp",
            Self::Composite => "Composite",
            Self::GpuWait => "GPU wait",
            Self::Texture => "Texture",
            Self::Snapshot => "Snapshot",
            Self::Timers => "Timers / IO",
        }
    }
}

/// One frame's drained measurements.
#[derive(Clone, Copy, Default, Debug)]
pub struct FrameSample {
    /// Self-time per stage, in milliseconds, indexed by [`Stage::index`].
    pub stages: [f32; STAGE_COUNT],
    /// Wall time from the oldest unpresented input event to the present that
    /// carried it. `None` when the frame was not driven by input.
    pub input_latency_ms: Option<f32>,
}

impl FrameSample {
    /// Total instrumented time this frame (everything except the residual).
    #[must_use]
    pub fn measured_ms(&self) -> f32 {
        self.stages.iter().sum()
    }
}

struct Profile {
    /// Live holders - one per visible overlay. Measurement runs while this is
    /// non-zero, so with two documents showing the overlay, hiding one doesn't
    /// silently zero the other's readings.
    holders: u32,
    totals: [f64; STAGE_COUNT],
    /// Open spans: `(stage index, entered at, time already spent in children)`.
    stack: Vec<(usize, Instant, f64)>,
    /// Arrival of the oldest input event not yet reflected on screen.
    pending_input: Option<Instant>,
    last_latency: Option<f32>,
}

impl Profile {
    const fn new() -> Self {
        Self {
            holders: 0,
            totals: [0.0; STAGE_COUNT],
            stack: Vec::new(),
            pending_input: None,
            last_latency: None,
        }
    }

    const fn enabled(&self) -> bool {
        self.holders > 0
    }

    fn reset(&mut self) {
        self.totals = [0.0; STAGE_COUNT];
        self.stack.clear();
        self.pending_input = None;
        self.last_latency = None;
    }
}

thread_local! {
    static PROFILE: RefCell<Profile> = const { RefCell::new(Profile::new()) };
}

/// Start measuring for one holder. Must be balanced by [`release`].
pub fn retain() {
    PROFILE.with(|p| {
        let mut p = p.borrow_mut();
        p.holders += 1;
        if p.holders == 1 {
            p.reset();
        }
    });
}

/// Drop one holder's claim. Measurement stops once the last one goes.
pub fn release() {
    PROFILE.with(|p| {
        let mut p = p.borrow_mut();
        p.holders = p.holders.saturating_sub(1);
        if p.holders == 0 {
            p.reset();
        }
    });
}

#[must_use]
pub fn enabled() -> bool {
    PROFILE.with(|p| p.borrow().enabled())
}

/// Open a span for `stage`. The returned guard closes it when dropped.
#[must_use = "the span ends when the guard is dropped"]
pub fn span(stage: Stage) -> Span {
    let open = PROFILE.with(|p| {
        let mut p = p.borrow_mut();
        if !p.enabled() {
            return false;
        }
        p.stack.push((stage.index(), Instant::now(), 0.0));
        true
    });
    Span { open }
}

/// RAII guard returned by [`span`].
pub struct Span {
    open: bool,
}

impl Drop for Span {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        let now = Instant::now();
        PROFILE.with(|p| {
            let mut p = p.borrow_mut();
            let Some((idx, started, children_ms)) = p.stack.pop() else {
                return;
            };
            let elapsed_ms = now.duration_since(started).as_secs_f64() * 1000.0;
            p.totals[idx] += (elapsed_ms - children_ms).max(0.0);
            if let Some(parent) = p.stack.last_mut() {
                parent.2 += elapsed_ms;
            }
        });
    }
}

/// Note that an input event just arrived. Only the first one after a present
/// counts, so the latency reading is the age of the oldest unshown input.
pub fn note_input() {
    PROFILE.with(|p| {
        let mut p = p.borrow_mut();
        if p.enabled() && p.pending_input.is_none() {
            p.pending_input = Some(Instant::now());
        }
    });
}

/// Note that the frame carrying the pending input reached GTK.
pub fn note_presented() {
    PROFILE.with(|p| {
        let mut p = p.borrow_mut();
        if let Some(t) = p.pending_input.take() {
            p.last_latency = Some(t.elapsed().as_secs_f32() * 1000.0);
        }
    });
}

/// Drain the accumulated stage totals and reset for the next frame.
pub fn take_frame() -> FrameSample {
    PROFILE.with(|p| {
        let mut p = p.borrow_mut();
        let mut stages = [0.0f32; STAGE_COUNT];
        for (out, total) in stages.iter_mut().zip(p.totals.iter()) {
            #[allow(clippy::cast_possible_truncation)]
            {
                *out = *total as f32;
            }
        }
        p.totals = [0.0; STAGE_COUNT];
        FrameSample {
            stages,
            input_latency_ms: p.last_latency.take(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy(ms: u64) {
        let until = Instant::now() + std::time::Duration::from_millis(ms);
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    }

    #[test]
    fn disabled_by_default_records_nothing() {
        {
            let _s = span(Stage::Composite);
            busy(2);
        }
        assert!(!enabled());
        assert!(take_frame().measured_ms().abs() < 1e-6);
    }

    #[test]
    fn last_holder_wins_not_the_first_to_leave() {
        // Two documents showing the overlay: hiding one must not stop the
        // other's measurements.
        retain();
        retain();
        release();
        assert!(enabled(), "still one holder left");
        release();
        assert!(!enabled());
        // Over-releasing must not underflow into a permanently enabled state.
        release();
        assert!(!enabled());
    }

    #[test]
    fn nested_spans_attribute_self_time_only() {
        retain();
        {
            let _outer = span(Stage::Composite);
            busy(4);
            let _inner = span(Stage::GpuWait);
            busy(6);
        }
        assert_eq!(Stage::ALL.len(), STAGE_COUNT);
        let sample = take_frame();
        let composite = sample.stages[Stage::Composite.index()];
        let gpu_wait = sample.stages[Stage::GpuWait.index()];
        // The outer span ran ~10ms wall but only ~4ms of it was its own.
        assert!((3.0..6.0).contains(&composite), "composite {composite}");
        assert!((5.0..9.0).contains(&gpu_wait), "gpu_wait {gpu_wait}");
        release();
    }

    #[test]
    fn take_frame_resets_totals() {
        retain();
        {
            let _s = span(Stage::Brush);
            busy(2);
        }
        assert!(take_frame().measured_ms() > 0.0);
        assert!(take_frame().measured_ms().abs() < 1e-6);
        release();
    }

    #[test]
    fn latency_spans_first_input_to_present() {
        retain();
        note_input();
        busy(5);
        note_input(); // later events must not reset the clock
        busy(3);
        note_presented();
        let latency = take_frame().input_latency_ms.expect("latency recorded");
        assert!(latency >= 7.0, "latency {latency}");
        // Drained: a frame with no input reports none.
        note_presented();
        assert!(take_frame().input_latency_ms.is_none());
        release();
    }
}
