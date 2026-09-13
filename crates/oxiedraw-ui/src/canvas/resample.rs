//! Time-based resampling of pointer input for the render pump.
//!
//! A pen reports at its own rate (an Intuos S about 133 times a second) while
//! the pump paints at the display's, so rendering the newest sample advances
//! the view in uneven steps - most frames get a sample's worth of motion and
//! every fifth gets none. [`PointerResampler`] answers where the pointer was
//! at a given instant instead, interpolating between the two newest samples,
//! so every frame advances the same amount whatever the input rate. Event
//! times are the compositor's milliseconds; [`EventClock`] maps them onto this
//! process's clock.

use std::time::Instant;

use oxiedraw_utils::geometry::Point;

/// How far behind "now" the rendered moment sits, in input intervals. Half an
/// interval leaves the average latency where rendering the newest sample put it.
const LEAD_FRACTION: f64 = 0.5;
const MAX_EXTRAPOLATION_FRACTION: f64 = 0.5;
/// Intervals over which an overshoot unwinds: slow enough that one late sample
/// barely walks the position back, quick enough that a pointer which has
/// stopped settles onto its last sample instead of sitting ahead of it.
const EASE_BACK_INTERVALS: f64 = 2.0;
const INTERVAL_SMOOTHING: f64 = 0.3;
const OFFSET_WINDOW_MS: f64 = 500.0;

pub(super) struct EventClock {
    epoch: Instant,
    offset_ms: Option<f64>,
    window_min_ms: Option<f64>,
    window_started_ms: f64,
}

impl EventClock {
    pub(super) fn new() -> Self {
        Self {
            epoch: Instant::now(),
            offset_ms: None,
            window_min_ms: None,
            window_started_ms: 0.0,
        }
    }

    pub(super) fn reset(&mut self) {
        self.offset_ms = None;
        self.window_min_ms = None;
        self.window_started_ms = self.local_ms();
    }

    fn local_ms(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * 1000.0
    }

    pub(super) fn observe(&mut self, event_ms: f64) {
        let now = self.local_ms();
        let delay = now - event_ms;
        self.offset_ms = Some(self.offset_ms.map_or(delay, |o| o.min(delay)));
        self.window_min_ms = Some(self.window_min_ms.map_or(delay, |m| m.min(delay)));
        // Re-based per window: an offset that came out too small would
        // otherwise hold for the rest of the interaction, putting every
        // rendered moment past the newest sample and stopping the resampling.
        if now - self.window_started_ms >= OFFSET_WINDOW_MS {
            self.offset_ms = self.window_min_ms.take();
            self.window_started_ms = now;
        }
    }

    pub(super) fn now_event_ms(&self) -> Option<f64> {
        self.offset_ms.map(|o| self.local_ms() - o)
    }
}

pub(super) struct PointerResampler {
    clock: EventClock,
    prev: Option<(f64, Point)>,
    last: Option<(f64, Point)>,
    interval_ms: Option<f64>,
}

impl PointerResampler {
    pub(super) fn new() -> Self {
        Self {
            clock: EventClock::new(),
            prev: None,
            last: None,
            interval_ms: None,
        }
    }

    pub(super) fn begin(&mut self) {
        self.clock.reset();
        self.prev = None;
        self.last = None;
        self.interval_ms = None;
    }

    pub(super) fn push(&mut self, event_ms: f64, position: Point) {
        self.clock.observe(event_ms);
        self.push_at(event_ms, position);
    }

    fn push_at(&mut self, t: f64, position: Point) {
        if let Some((t_last, _)) = self.last {
            let dt = t - t_last;
            if dt < 0.5 {
                self.last = Some((t_last, position));
                return;
            }
            self.interval_ms = Some(
                self.interval_ms
                    .map_or(dt, |i| (dt - i).mul_add(INTERVAL_SMOOTHING, i)),
            );
        }
        self.prev = self.last;
        self.last = Some((t, position));
    }

    /// The position to render for this frame.
    pub(super) fn sample(&self) -> Option<Point> {
        let now = self.clock.now_event_ms()?;
        self.position_at(now)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn position_at(&self, now: f64) -> Option<Point> {
        let (t1, p1) = self.last?;
        let (Some((t0, p0)), Some(interval)) = (self.prev, self.interval_ms) else {
            return Some(p1);
        };
        let interval = interval.max(1.0);
        let segment = (t1 - t0).max(1.0);
        let target = now - interval * LEAD_FRACTION;
        if target <= t1 {
            let f = ((target - t0) / segment).clamp(0.0, 1.0);
            return Some(p0.lerp(p1, f as f32));
        }
        let max_ahead = interval * MAX_EXTRAPOLATION_FRACTION;
        let ahead = target - t1;
        let reach = if ahead <= max_ahead {
            ahead
        } else {
            let unwound = (ahead - max_ahead) / (interval * EASE_BACK_INTERVALS);
            max_ahead * (1.0 - unwound).max(0.0)
        };
        let vx = f64::from(p1.x - p0.x) / segment;
        let vy = f64::from(p1.y - p0.y) / segment;
        Some(Point::new(
            p1.x + (vx * reach) as f32,
            p1.y + (vy * reach) as f32,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_near(p: Option<Point>, x: f32, y: f32) {
        let p = p.expect("position");
        assert!(
            (p.x - x).abs() < 1e-3 && (p.y - y).abs() < 1e-3,
            "got {p:?}, want ({x}, {y})"
        );
    }

    /// Two samples 10 ms apart along x.
    fn two_samples() -> PointerResampler {
        let mut r = PointerResampler::new();
        r.push_at(0.0, Point::new(0.0, 0.0));
        r.push_at(10.0, Point::new(10.0, 0.0));
        r
    }

    #[test]
    fn single_sample_is_returned_as_is() {
        let mut r = PointerResampler::new();
        r.push_at(5.0, Point::new(3.0, 4.0));
        assert_near(r.position_at(100.0), 3.0, 4.0);
    }

    #[test]
    fn interpolates_half_an_interval_behind_now() {
        let r = two_samples();
        assert_near(r.position_at(15.0), 10.0, 0.0);
        assert_near(r.position_at(12.5), 7.5, 0.0);
        assert_near(r.position_at(10.0), 5.0, 0.0);
        assert_near(r.position_at(2.0), 0.0, 0.0);
    }

    #[test]
    fn extrapolation_is_capped_then_unwinds_onto_the_last_sample() {
        let r = two_samples();
        assert_near(r.position_at(20.0), 15.0, 0.0);
        assert_near(r.position_at(22.0), 14.5, 0.0);
        assert_near(r.position_at(25.0), 13.75, 0.0);
        assert_near(r.position_at(45.0), 10.0, 0.0);
        assert_near(r.position_at(90.0), 10.0, 0.0);
    }

    #[test]
    fn same_millisecond_replaces_the_position() {
        let mut r = two_samples();
        r.push_at(10.0, Point::new(11.0, 0.0));
        assert_near(r.position_at(15.0), 11.0, 0.0);
    }

    #[test]
    fn interval_estimate_is_smoothed() {
        let mut r = two_samples();
        r.push_at(12.0, Point::new(12.0, 0.0));
        let interval = r.interval_ms.expect("interval");
        assert!((interval - 7.6).abs() < 1e-9, "got {interval}");
    }

    #[test]
    fn begin_forgets_samples() {
        let mut r = two_samples();
        r.begin();
        assert!(r.position_at(20.0).is_none());
    }
}
