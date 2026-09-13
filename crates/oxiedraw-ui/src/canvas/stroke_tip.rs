//! Time-released stroke dabs, so the tip advances evenly at any pen rate.
//!
//! The brush engine emits a spline segment's dabs all at once, when the sample
//! after it arrives. Stamped straight away they move the tip a whole segment
//! per sample, which at a display rate above the pen's reads as a stall every
//! few frames. [`DeferredDabs`] holds them with a reveal time instead and the
//! render pump stamps what is due each frame. Pen-up and anything that reads
//! the stroke buffer flush the rest first, so the committed stroke is
//! unchanged.

use std::collections::VecDeque;

use oxiedraw_core::brush_engine::{BrushFamily, Dab, PaintTarget};
use oxiedraw_core::canvas::Canvas;

use super::resample::EventClock;

/// How much of the time the pen took over a segment its replay takes.
const SPREAD: f64 = 1.0;
/// Ceiling on that replay: when the input stream stalls, one segment arrives
/// covering a long stretch of drawing and the tip has to catch the pen up
/// rather than crawl through it.
const MAX_SPAN_MS: f64 = 16.0;

/// Dabs that came due together, in stroke order.
pub(super) struct DueRun {
    pub(super) family: BrushFamily,
    pub(super) dabs: Vec<Dab>,
}

struct Run {
    family: BrushFamily,
    dabs: VecDeque<Dab>,
    reveal_ms: VecDeque<f64>,
}

pub(super) struct DeferredDabs {
    clock: EventClock,
    family: Option<BrushFamily>,
    now_ms: f64,
    runs: VecDeque<Run>,
}

impl DeferredDabs {
    pub(super) fn new() -> Self {
        Self {
            clock: EventClock::new(),
            family: None,
            now_ms: 0.0,
            runs: VecDeque::new(),
        }
    }

    pub(super) fn begin(&mut self) {
        self.runs.clear();
        self.family = None;
        self.now_ms = 0.0;
        self.clock.reset();
    }

    /// Call before the engine sees the sample stamped `event_ms`.
    pub(super) fn begin_sample(&mut self, event_ms: f64) {
        self.clock.observe(event_ms);
        self.now_ms = event_ms;
    }

    pub(super) fn take_due(&mut self) -> Vec<DueRun> {
        let now = self.clock.now_event_ms();
        now.map_or_else(Vec::new, |now| self.take_until(now))
    }

    pub(super) fn take_all(&mut self) -> Vec<DueRun> {
        self.take_until(f64::INFINITY)
    }

    fn take_until(&mut self, now: f64) -> Vec<DueRun> {
        let mut out = Vec::new();
        while let Some(run) = self.runs.front_mut() {
            let mut dabs = Vec::new();
            while run.reveal_ms.front().is_some_and(|&t| t <= now) {
                run.reveal_ms.pop_front();
                if let Some(dab) = run.dabs.pop_front() {
                    dabs.push(dab);
                }
            }
            if !dabs.is_empty() {
                out.push(DueRun {
                    family: run.family.clone(),
                    dabs,
                });
            }
            if run.dabs.is_empty() {
                self.runs.pop_front();
            } else {
                break;
            }
        }
        out
    }

    fn push_run(&mut self, dabs: &[Dab], reveal_ms: impl Fn(usize) -> f64) {
        if dabs.is_empty() {
            return;
        }
        let family = self.family.clone().unwrap_or(BrushFamily::SoftRound);
        self.runs.push_back(Run {
            family,
            dabs: dabs.iter().copied().collect(),
            reveal_ms: (0..dabs.len()).map(reveal_ms).collect(),
        });
    }
}

impl PaintTarget for DeferredDabs {
    fn set_family(&mut self, family: &BrushFamily) {
        self.family = Some(family.clone());
    }

    /// Dabs without a segment span (the stroke's first dab) are due at once.
    fn paint_dabs(&mut self, dabs: &[Dab]) {
        let now = self.now_ms;
        self.push_run(dabs, |_| now);
    }

    #[allow(clippy::cast_precision_loss)]
    fn paint_segment_dabs(&mut self, dabs: &[Dab], from_ms: u64, to_ms: u64) {
        let span = (to_ms.saturating_sub(from_ms) as f64 * SPREAD).min(MAX_SPAN_MS);
        let start = self.now_ms;
        let count = dabs.len() as f64;
        self.push_run(dabs, |i| (i as f64 + 1.0).mul_add(span / count, start));
    }
}

/// Stamp released runs into the stroke buffer, in order.
pub(super) fn stamp_runs(canvas: &mut Canvas, runs: &[DueRun]) {
    if runs.is_empty() {
        return;
    }
    if let Err(e) = canvas.stamp(|target| {
        for run in runs {
            target.set_family(&run.family);
            target.paint_dabs(&run.dabs);
        }
    }) {
        tracing::error!(error = %e, "stamping released stroke dabs failed");
    }
}

#[cfg(test)]
mod tests {
    use oxiedraw_core::color::Color;
    use oxiedraw_utils::geometry::Point;

    use super::*;

    #[allow(clippy::cast_precision_loss)]
    fn dabs(n: usize) -> Vec<Dab> {
        (0..n)
            .map(|i| Dab::round(Point::new(i as f32, 0.0), 1.0, Color::new(0, 0, 0)))
            .collect()
    }

    /// The x coordinates of every released dab, run after run.
    #[allow(clippy::cast_possible_truncation)]
    fn xs(runs: &[DueRun]) -> Vec<i32> {
        runs.iter()
            .flat_map(|r| r.dabs.iter().map(|d| d.center.x as i32))
            .collect()
    }

    #[test]
    fn first_dab_is_due_at_once() {
        let mut d = DeferredDabs::new();
        d.now_ms = 100.0;
        d.set_family(&BrushFamily::SoftRound);
        d.paint_dabs(&dabs(1));
        assert_eq!(xs(&d.take_until(100.0)), vec![0]);
        assert!(d.take_all().is_empty());
    }

    #[test]
    fn segment_replays_over_the_time_it_took() {
        let mut d = DeferredDabs::new();
        d.now_ms = 100.0;
        d.set_family(&BrushFamily::SoftRound);
        // An 8 ms segment emitted at 100: reveals at 102, 104, 106, 108.
        d.paint_segment_dabs(&dabs(4), 90, 98);
        assert!(d.take_until(101.0).is_empty());
        assert_eq!(xs(&d.take_until(105.0)), vec![0, 1]);
        assert_eq!(xs(&d.take_until(108.0)), vec![2, 3]);
        assert!(d.take_all().is_empty());
    }

    #[test]
    fn runs_keep_stroke_order_and_family() {
        let mut d = DeferredDabs::new();
        d.now_ms = 0.0;
        d.set_family(&BrushFamily::SoftRound);
        d.paint_segment_dabs(&dabs(2), 0, 10);
        d.now_ms = 10.0;
        d.set_family(&BrushFamily::Pixel);
        d.paint_segment_dabs(&dabs(2), 10, 20);
        let due = d.take_until(15.0);
        assert_eq!(xs(&due), vec![0, 1, 0]);
        assert!(matches!(due[0].family, BrushFamily::SoftRound));
        assert!(matches!(due[1].family, BrushFamily::Pixel));
        assert_eq!(xs(&d.take_all()), vec![1]);
        assert!(d.take_all().is_empty());
    }

    #[test]
    fn a_stalled_input_stream_does_not_make_the_tip_crawl() {
        let mut d = DeferredDabs::new();
        d.now_ms = 200.0;
        d.set_family(&BrushFamily::SoftRound);
        // 150 ms of drawing in one segment: replayed over the ceiling instead.
        d.paint_segment_dabs(&dabs(4), 50, 200);
        assert!(d.take_until(203.0).is_empty());
        assert_eq!(xs(&d.take_until(208.0)), vec![0, 1]);
        assert_eq!(xs(&d.take_until(216.0)), vec![2, 3]);
    }
}
