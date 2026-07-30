//! Per-parameter brush dynamics.
//!
//! A `Dynamics` value carries an optional `Mapping` for each dab
//! parameter that can be driven by an input signal (pressure, speed,
//! direction, distance, random). Each mapping pipes the signal through
//! a normalised `Curve` and rescales the result into an output range.
//!
//! The evaluator (`evaluate`) is the single integration point: brushes
//! call it once per dab spawn and the relevant fields on the base `Dab`
//! are mutated in place. When `Dynamics::any_active() == false` the
//! caller is expected to skip the call entirely - the dab is then
//! produced from the preset defaults with zero per-sample CPU cost.

use std::f32::consts::TAU;

use serde::{Deserialize, Serialize};

use super::Dab;

/// Signals the dynamics evaluator can read at each dab spawn point.
/// All values are pre-normalised into the curve's `0..=1` input space.
#[derive(Debug, Clone, Copy)]
pub struct SpawnInput {
    /// Pen pressure already clamped to `0..=1`.
    pub pressure: f32,
    /// Stroke speed normalised against `SPEED_NORM_PX_PER_MS`.
    pub speed: f32,
    /// Stroke direction packed into `0..=1` (one full revolution).
    pub direction: f32,
    /// Cumulative distance along the stroke divided by `base_size`,
    /// then taken `frac` of so it wraps every brush diameter.
    pub distance: f32,
    /// Deterministic per-dab random, `0..=1`.
    pub random: f32,
    /// Real tablet pen barrel rotation (twist axis) packed into `0..=1`
    /// (one full revolution). `0` for pens that don't expose rotation.
    pub pen_rotation: f32,
    /// Real tablet pen tilt azimuth - the compass direction the pen is
    /// leaning toward, packed into `0..=1` (one full revolution). `0`
    /// for pens that don't expose tilt or that are perfectly upright.
    pub angle: f32,
}

/// How `SPEED_NORM_PX_PER_MS` is set: 5 canvas pixels per millisecond
/// is a brisk paint motion; faster than that saturates to 1.0.
const SPEED_NORM_PX_PER_MS: f32 = 5.0;

/// Build a `SpawnInput` from raw stroke-time values. Centralised so the
/// stamp code and tests share the same normalisation.
///
/// `pen_rotation_rad` is the tablet's barrel-twist axis (radians, any
/// range - re-normalised here). `tilt_x` / `tilt_y` are the GDK tilt
/// axes (each `-1..=1`); their `atan2` becomes the `angle` input. Pass
/// zeros when the device doesn't expose those axes.
#[allow(clippy::too_many_arguments)]
pub fn make_spawn_input(
    pressure: f32,
    speed_px_per_ms: f32,
    direction_rad: f32,
    cumulative_distance_px: f32,
    base_size_px: f32,
    random_unit: f32,
    pen_rotation_rad: f32,
    tilt_x: f32,
    tilt_y: f32,
) -> SpawnInput {
    let dir_unit = direction_rad.rem_euclid(TAU) / TAU;
    let dist_unit = if base_size_px > f32::EPSILON {
        (cumulative_distance_px / base_size_px).fract().abs()
    } else {
        0.0
    };
    let rot_unit = pen_rotation_rad.rem_euclid(TAU) / TAU;
    // Tilt-azimuth: which way the pen is leaning. When the pen is
    // upright (tilt_x = tilt_y = 0) `atan2(0, 0)` returns 0, which
    // matches "no tilt -> no rotation contribution".
    let angle_unit = if tilt_x.abs() < f32::EPSILON && tilt_y.abs() < f32::EPSILON {
        0.0
    } else {
        tilt_y.atan2(tilt_x).rem_euclid(TAU) / TAU
    };
    SpawnInput {
        pressure: pressure.clamp(0.0, 1.0),
        speed: (speed_px_per_ms / SPEED_NORM_PX_PER_MS).clamp(0.0, 1.0),
        direction: dir_unit,
        distance: dist_unit,
        random: random_unit.clamp(0.0, 1.0),
        pen_rotation: rot_unit,
        angle: angle_unit,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DynSource {
    Pressure,
    Speed,
    Direction,
    Distance,
    Random,
    /// Real tablet pen barrel rotation (twist axis).
    PenRotation,
    /// Synthesised "follows the stroke direction" alias of `Direction`;
    /// distinct UI-wise so users can wire it into rotation parameters
    /// without confusing it with the literal `Direction` signal.
    FakePenRotation,
    /// Real tablet pen tilt azimuth (which way the pen is leaning).
    Angle,
    /// Synthesised "follows the stroke direction" alias of `Direction`,
    /// counterpart to `Angle` for tablets that don't report tilt.
    FakeAngle,
}

impl DynSource {
    pub const fn read(self, input: &SpawnInput) -> f32 {
        match self {
            Self::Pressure => input.pressure,
            Self::Speed => input.speed,
            // Fake rotation/angle just follow stroke direction; the
            // separate names exist so the UI exposes them as
            // brush-rotation-flavoured signals.
            Self::Direction | Self::FakePenRotation | Self::FakeAngle => input.direction,
            Self::Distance => input.distance,
            Self::Random => input.random,
            Self::PenRotation => input.pen_rotation,
            Self::Angle => input.angle,
        }
    }
}

/// Piecewise-linear curve sampled in `0..=1` input space, output also
/// in `0..=1`. Points must be sorted by `x` and bracket `0..=1`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Curve {
    points: Vec<(f32, f32)>,
}

impl Curve {
    /// `y = x`. The "no-op" curve.
    pub fn linear() -> Self {
        Self {
            points: vec![(0.0, 0.0), (1.0, 1.0)],
        }
    }

    /// Constant `y`. Useful as a building block for `Mapping::range`.
    pub fn flat(y: f32) -> Self {
        let y = y.clamp(0.0, 1.0);
        Self {
            points: vec![(0.0, y), (1.0, y)],
        }
    }

    /// Build from an arbitrary point list. Returns `None` if the
    /// invariants (>= 2 points, sorted x, all values in `0..=1`) fail.
    pub fn from_points(points: Vec<(f32, f32)>) -> Option<Self> {
        if points.len() < 2 {
            return None;
        }
        for w in points.windows(2) {
            if w[0].0 > w[1].0 {
                return None;
            }
        }
        for &(x, y) in &points {
            if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
                return None;
            }
        }
        Some(Self { points })
    }

    pub fn sample(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        let pts = &self.points;
        if x <= pts[0].0 {
            return pts[0].1;
        }
        if x >= pts[pts.len() - 1].0 {
            return pts[pts.len() - 1].1;
        }
        for w in pts.windows(2) {
            let (x0, y0) = w[0];
            let (x1, y1) = w[1];
            if x >= x0 && x <= x1 {
                let span = (x1 - x0).max(f32::EPSILON);
                let t = (x - x0) / span;
                return (y1 - y0).mul_add(t, y0);
            }
        }
        pts[pts.len() - 1].1
    }
}

impl Default for Curve {
    fn default() -> Self {
        Self::linear()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mapping {
    pub source: DynSource,
    pub curve: Curve,
    /// Output range `(min, max)`. Curve `y` is interpolated through this.
    pub range: (f32, f32),
    pub invert: bool,
}

impl Mapping {
    /// Convenience: straight pressure-to-curve mapping that emits
    /// `0..=1`. Used for the legacy pressure -> size response.
    pub fn pressure_linear() -> Self {
        Self {
            source: DynSource::Pressure,
            curve: Curve::linear(),
            range: (0.0, 1.0),
            invert: false,
        }
    }

    pub fn apply(&self, input: &SpawnInput) -> f32 {
        let mut x = self.source.read(input).clamp(0.0, 1.0);
        if self.invert {
            x = 1.0 - x;
        }
        let y = self.curve.sample(x);
        (self.range.1 - self.range.0).mul_add(y, self.range.0)
    }
}

/// Per-parameter dynamics. Each field is `Some` to opt in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Dynamics {
    /// Multiplier on base diameter, range typically `(0.0, 1.0)`.
    #[serde(default)]
    pub size: Option<Mapping>,
    /// Per-dab coverage multiplier, `(0.0, 1.0)`.
    #[serde(default)]
    pub flow: Option<Mapping>,
    /// Additive rotation in radians, typical range `(0, TAU)` or
    /// `(-PI, PI)` for jitter.
    #[serde(default)]
    pub rotation: Option<Mapping>,
    /// Random offset applied to dab centre in pixels, typically
    /// `(0.0, base_size)`.
    #[serde(default)]
    pub scatter: Option<Mapping>,
    /// Per-segment override of the preset's static `spacing_ratio`.
    /// `None` keeps the fixed preset value; `Some(mapping)` evaluates
    /// the mapping once per segment to drive the dab step. Typical
    /// range `(0.05, 1.0)` - the renderer clamps to a floor that
    /// guarantees overlapping dabs.
    #[serde(default)]
    pub spacing: Option<Mapping>,
    /// Smudge family: colour-pickup rate, `0..=1`.
    #[serde(default)]
    pub smudge_rate: Option<Mapping>,
    /// Smudge family: paint-colour mix rate, `0..=1`.
    #[serde(default)]
    pub color_rate: Option<Mapping>,
}

impl Dynamics {
    pub const fn any_active(&self) -> bool {
        self.size.is_some()
            || self.flow.is_some()
            || self.rotation.is_some()
            || self.scatter.is_some()
            || self.spacing.is_some()
            || self.smudge_rate.is_some()
            || self.color_rate.is_some()
    }
}

/// Mutate `dab` according to `dynamics`. The caller has already
/// populated `dab` with the preset defaults (radius from base size,
/// `flow = 1`, etc.); this routine only adjusts the fields with an
/// active mapping. `scatter_seed` provides two independent random
/// values for the x/y offset so scatter doesn't land on a diagonal.
pub fn evaluate(
    dynamics: &Dynamics,
    input: &SpawnInput,
    base_size: f32,
    scatter_seed: (f32, f32),
    dab: &mut Dab,
) {
    if let Some(m) = &dynamics.size {
        // Size is a multiplier on diameter; radius is half of that.
        dab.radius = (base_size * m.apply(input) * 0.5).max(0.0);
    }
    if let Some(m) = &dynamics.flow {
        dab.flow = m.apply(input).clamp(0.0, 1.0);
    }
    if let Some(m) = &dynamics.rotation {
        dab.rotation += m.apply(input);
    }
    if let Some(m) = &dynamics.scatter {
        let radius = m.apply(input);
        let (rx, ry) = scatter_seed;
        dab.center.x += rx.mul_add(2.0, -1.0) * radius;
        dab.center.y += ry.mul_add(2.0, -1.0) * radius;
    }
    if let Some(m) = &dynamics.smudge_rate {
        dab.smudge_rate = m.apply(input).clamp(0.0, 1.0);
    }
    if let Some(m) = &dynamics.color_rate {
        dab.color_rate = m.apply(input).clamp(0.0, 1.0);
    }
    // `Color` does not yet have an alpha channel, so hue / sat / val
    // dynamics are deferred until the stroke buffer is promoted to RGBA
    // (see `renderer/mod.rs`). Until then, per-dab colour is the stroke
    // colour as set by the preset.
    let _ = dab.color;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::color::Color;

    fn spawn(pressure: f32) -> SpawnInput {
        SpawnInput {
            pressure,
            speed: 0.0,
            direction: 0.0,
            distance: 0.0,
            random: 0.0,
            pen_rotation: 0.0,
            angle: 0.0,
        }
    }

    #[test]
    fn curve_linear_passes_through() {
        let c = Curve::linear();
        assert!((c.sample(0.0) - 0.0).abs() < 1e-6);
        assert!((c.sample(0.5) - 0.5).abs() < 1e-6);
        assert!((c.sample(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn curve_clamps_outside() {
        let c = Curve::linear();
        assert!((c.sample(-1.0) - 0.0).abs() < 1e-6);
        assert!((c.sample(2.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn curve_piecewise_lerps() {
        let c = Curve::from_points(vec![(0.0, 0.0), (0.5, 0.2), (1.0, 1.0)]).unwrap();
        assert!((c.sample(0.25) - 0.1).abs() < 1e-6);
        assert!((c.sample(0.75) - 0.6).abs() < 1e-6);
    }

    #[test]
    fn mapping_invert_flips_input() {
        let m = Mapping {
            source: DynSource::Pressure,
            curve: Curve::linear(),
            range: (0.0, 1.0),
            invert: true,
        };
        assert!((m.apply(&spawn(0.25)) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn mapping_range_rescales_output() {
        let m = Mapping {
            source: DynSource::Pressure,
            curve: Curve::flat(1.0),
            range: (0.2, 0.8),
            invert: false,
        };
        assert!((m.apply(&spawn(0.5)) - 0.8).abs() < 1e-6);
    }

    #[test]
    fn evaluate_size_scales_radius() {
        let mut dab = Dab::round(
            oxiedraw_utils::geometry::Point::new(0.0, 0.0),
            10.0,
            Color::BLACK,
        );
        let dynamics = Dynamics {
            size: Some(Mapping::pressure_linear()),
            ..Default::default()
        };
        evaluate(&dynamics, &spawn(0.5), 20.0, (0.0, 0.0), &mut dab);
        // base_size 20, pressure 0.5, linear -> radius = 20 * 0.5 * 0.5 = 5
        assert!((dab.radius - 5.0).abs() < 1e-6);
    }

    #[test]
    fn evaluate_no_active_is_noop() {
        let mut dab = Dab::round(
            oxiedraw_utils::geometry::Point::new(0.0, 0.0),
            7.0,
            Color::BLACK,
        );
        let snapshot = dab;
        evaluate(&Dynamics::default(), &spawn(0.5), 20.0, (0.5, 0.5), &mut dab);
        assert!((dab.radius - snapshot.radius).abs() < 1e-6);
        assert!((dab.flow - snapshot.flow).abs() < 1e-6);
        assert!((dab.rotation - snapshot.rotation).abs() < 1e-6);
        assert!((dab.center.x - snapshot.center.x).abs() < 1e-6);
    }
}
