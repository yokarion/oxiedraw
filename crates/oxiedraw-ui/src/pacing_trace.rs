//! Opt-in frame pacing trace, enabled with `OXIEDRAW_PACING_TRACE=1`.
//!
//! While the render pump runs it logs one summary line per second: how evenly
//! the app painted, what each present cost, and - from the compositor's
//! presentation feedback - how evenly those frames reached the screen. Uneven
//! presentation with even painting points at the GPU or the compositor, uneven
//! painting at the app or GTK's frame clock, and cycles without feedback are
//! frames GTK never committed.

use std::cell::Cell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use relm4::gtk::gdk;

/// A presentation gap of at least this many refresh intervals is a skipped frame.
const SKIP_RATIO: f64 = 1.5;
const SHORT_RATIO: f64 = 0.5;
const FEEDBACK_TIMEOUT: Duration = Duration::from_millis(150);
const RECENT_TICKS: usize = 256;
/// Pump frames further apart than this belong to different interactions, so
/// the gap between them is idle time rather than a skipped refresh.
const INTERACTION_GAP: Duration = Duration::from_millis(100);
const SUMMARY_EVERY: Duration = Duration::from_secs(1);
const ANOMALY_LINES_PER_SUMMARY: u32 = 6;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static SNAPSHOTS: Cell<u32> = const { Cell::new(0) };
    static INPUTS: Cell<u32> = const { Cell::new(0) };
}

/// The counters below sit on the paint and input paths, so with the trace off
/// they must cost one flag read and nothing else.
fn enabled() -> bool {
    ENABLED.with(Cell::get)
}

pub(crate) fn note_snapshot() {
    if enabled() {
        SNAPSHOTS.with(|s| s.set(s.get().wrapping_add(1)));
    }
}

pub(crate) fn note_input() {
    if enabled() {
        INPUTS.with(|s| s.set(s.get().wrapping_add(1)));
    }
}

fn take_snapshots() -> u32 {
    SNAPSHOTS.with(|s| s.replace(0))
}

fn take_inputs() -> u32 {
    INPUTS.with(|s| s.replace(0))
}

struct TickRecord {
    frame_counter: i64,
    at: Instant,
    paint_gap_us: u64,
    present_us: u64,
}

#[derive(Default)]
struct Window {
    paints: u32,
    presents: u32,
    paint_gaps_us: Vec<u64>,
    present_max_us: u64,
    present_sum_us: u64,
    shown: u32,
    discarded: u32,
    no_feedback: u32,
    gap_regular: u32,
    gap_skipped: u32,
    gap_short: u32,
    latency_sum_us: i64,
    latency_count: u32,
    anomaly_lines: u32,
}

pub(crate) struct PacingTrace {
    last_tick: Option<Instant>,
    paint_gap_us: u64,
    recent: VecDeque<TickRecord>,
    next_feedback: Option<i64>,
    /// The last pump frame shown: when its tick ran, and its presentation time.
    last_shown: Option<(Instant, i64)>,
    refresh_us: i64,
    window_started: Instant,
    window: Window,
}

impl PacingTrace {
    pub(crate) fn from_env() -> Option<Self> {
        let on = std::env::var("OXIEDRAW_PACING_TRACE")
            .is_ok_and(|v| !matches!(v.trim(), "" | "0" | "false"));
        if !on {
            return None;
        }
        ENABLED.with(|e| e.set(true));
        tracing::info!("frame pacing trace on: one summary line per second while interacting");
        Some(Self {
            last_tick: None,
            paint_gap_us: 0,
            recent: VecDeque::with_capacity(RECENT_TICKS),
            next_feedback: None,
            last_shown: None,
            refresh_us: 0,
            window_started: Instant::now(),
            window: Window::default(),
        })
    }

    pub(crate) fn begin_tick(&mut self) {
        let now = Instant::now();
        self.paint_gap_us = self.last_tick.map_or(0, |t| micros(now.duration_since(t)));
        self.last_tick = Some(now);
    }

    /// `present_us` is 0 when the frame only moved the view.
    pub(crate) fn end_tick(&mut self, clock: &gdk::FrameClock, present_us: u64) {
        let frame_counter = clock.frame_counter();
        self.window.paints += 1;
        if self.paint_gap_us > 0 {
            self.window.paint_gaps_us.push(self.paint_gap_us);
        }
        if present_us > 0 {
            self.window.presents += 1;
            self.window.present_sum_us += present_us;
            self.window.present_max_us = self.window.present_max_us.max(present_us);
        }
        if self.recent.len() == RECENT_TICKS {
            self.recent.pop_front();
        }
        self.recent.push_back(TickRecord {
            frame_counter,
            at: Instant::now(),
            paint_gap_us: self.paint_gap_us,
            present_us,
        });

        self.consume_feedback(clock, frame_counter);

        if self.window_started.elapsed() >= SUMMARY_EVERY {
            self.log_summary();
            self.window = Window::default();
            self.window_started = Instant::now();
        }
    }

    fn consume_feedback(&mut self, clock: &gdk::FrameClock, frame_counter: i64) {
        // Feedback for a frame arrives a frame or two after it was painted.
        // Resuming from the oldest tick on record skips the thousands of frames
        // other widgets painted while the pump was idle.
        let newest = frame_counter - 2;
        let oldest = self.recent.front().map_or(newest, |r| r.frame_counter);
        let mut next = self.next_feedback.unwrap_or(newest).max(oldest);
        while next <= newest {
            let Some(tick_at) = self.tick_at(next) else {
                next += 1;
                continue;
            };
            if next < clock.history_start() {
                self.window.no_feedback += 1;
                next += 1;
                continue;
            }
            let Some(timings) = clock.timings(next) else {
                self.window.no_feedback += 1;
                next += 1;
                continue;
            };
            if !timings.is_complete() {
                if tick_at.elapsed() < FEEDBACK_TIMEOUT {
                    break;
                }
                self.window.no_feedback += 1;
                next += 1;
                continue;
            }
            self.record_shown(next, tick_at, &timings);
            next += 1;
        }
        self.next_feedback = Some(next);
    }

    fn tick_at(&self, frame_counter: i64) -> Option<Instant> {
        self.recent
            .iter()
            .find(|r| r.frame_counter == frame_counter)
            .map(|r| r.at)
    }

    fn record_shown(&mut self, frame_counter: i64, tick_at: Instant, timings: &gdk::FrameTimings) {
        let shown_at = timings.presentation_time();
        if shown_at <= 0 {
            self.window.discarded += 1;
            self.log_anomaly(frame_counter, "frame discarded by the compositor", 0, 0.0);
            return;
        }
        let refresh = timings.refresh_interval();
        if refresh > 0 {
            self.refresh_us = refresh;
        }
        self.window.shown += 1;
        self.window.latency_sum_us += shown_at - timings.frame_time();
        self.window.latency_count += 1;
        if let Some((prev_tick_at, prev_shown_at)) = self.last_shown
            && tick_at.duration_since(prev_tick_at) < INTERACTION_GAP
            && self.refresh_us > 0
        {
            let gap_us = shown_at - prev_shown_at;
            let ratio = signed_ms(gap_us) / signed_ms(self.refresh_us);
            if ratio >= SKIP_RATIO {
                self.window.gap_skipped += 1;
                self.log_anomaly(frame_counter, "skipped refresh before this frame", gap_us, ratio);
            } else if ratio < SHORT_RATIO {
                self.window.gap_short += 1;
            } else {
                self.window.gap_regular += 1;
            }
        }
        self.last_shown = Some((tick_at, shown_at));
    }

    fn log_anomaly(&mut self, frame_counter: i64, what: &str, gap_us: i64, ratio: f64) {
        if self.window.anomaly_lines >= ANOMALY_LINES_PER_SUMMARY {
            return;
        }
        self.window.anomaly_lines += 1;
        let (paint_gap_us, present_us) = self
            .recent
            .iter()
            .find(|r| r.frame_counter == frame_counter)
            .map_or((0, 0), |r| (r.paint_gap_us, r.present_us));
        tracing::info!(
            frame = frame_counter,
            gap_ms = round2(signed_ms(gap_us)),
            ratio = round2(ratio),
            paint_gap_ms = round2(ms(paint_gap_us)),
            present_ms = round2(ms(present_us)),
            "pacing: {what}"
        );
    }

    fn log_summary(&self) {
        let w = &self.window;
        let mut gaps = w.paint_gaps_us.clone();
        gaps.sort_unstable();
        let paint_gap_p99_ms = round2(ms(percentile(&gaps, 99)));
        let paint_gap_max_ms = round2(ms(gaps.last().copied().unwrap_or(0)));
        let present_avg_ms = if w.presents == 0 {
            0.0
        } else {
            round2(ms(w.present_sum_us) / f64::from(w.presents))
        };
        let latency_avg_ms = if w.latency_count == 0 {
            0.0
        } else {
            round2(signed_ms(w.latency_sum_us) / f64::from(w.latency_count))
        };
        tracing::info!(
            ticks = w.paints,
            snapshots = take_snapshots(),
            inputs = take_inputs(),
            presents = w.presents,
            view_only = w.paints - w.presents,
            shown = w.shown,
            discarded = w.discarded,
            no_feedback = w.no_feedback,
            regular = w.gap_regular,
            skipped = w.gap_skipped,
            doubled = w.gap_short,
            paint_gap_p99_ms,
            paint_gap_max_ms,
            present_avg_ms,
            present_max_ms = round2(ms(w.present_max_us)),
            latency_avg_ms,
            refresh_ms = round2(signed_ms(self.refresh_us)),
            "pacing"
        );
    }
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

#[allow(clippy::cast_precision_loss)]
fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

#[allow(clippy::cast_precision_loss)]
fn signed_ms(us: i64) -> f64 {
    us as f64 / 1000.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Nearest-rank percentile of an ascending slice; 0 when empty.
fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() - 1) * pct / 100;
    sorted[idx.min(sorted.len() - 1)]
}
