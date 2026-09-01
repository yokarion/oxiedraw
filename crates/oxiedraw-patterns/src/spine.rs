//! The drawn curve, turned into the coordinate system patterns grow in.
//!
//! A [`SpineField`] resamples the nodes at a fixed arc-length step and carries a
//! frame at every step. Generators work in ribbon space - `s` along the curve,
//! `n` across it, both in pixels of the rest length - and never see canvas
//! coordinates, so moving a node re-maps existing geometry rather than
//! regenerating it.

use oxiedraw_utils::geometry::Point;

use crate::params::{ModSource, Modulation};

/// Arc-length between frame samples, in canvas pixels. Fine enough that a
/// tight curve keeps its shape, coarse enough that a long stroke stays cheap.
pub const SAMPLE_STEP: f32 = 2.0;

/// Curve radius (px) at which [`ModSource::Curvature`] reads as fully bent.
const CURVATURE_REFERENCE: f32 = 60.0;

/// Wavelength (px) of the noise behind [`ModSource::Noise`].
const NOISE_WAVELENGTH: f32 = 64.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpineNode {
    pub pos: Point,
    /// Stylus pressure, `0..=1`. Devices with no pressure report 1.0.
    pub pressure: f32,
}

impl SpineNode {
    pub const fn new(pos: Point, pressure: f32) -> Self {
        Self { pos, pressure }
    }
}

/// Which side of the curve a pattern grows from. Sides are named as seen
/// walking along the stroke in the direction it was drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Side {
    #[default]
    Left,
    Right,
    /// Grow on both sides, each with its own randomness.
    Both,
}

impl Side {
    pub const ALL: &'static [Self] = &[Self::Left, Self::Right, Self::Both];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Both => "Both",
        }
    }

    /// Normal signs to generate for. `Both` yields two passes.
    pub const fn signs(self) -> &'static [f32] {
        match self {
            Self::Left => &[1.0],
            Self::Right => &[-1.0],
            Self::Both => &[1.0, -1.0],
        }
    }
}

/// The curve's state at one arc-length position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameSample {
    /// Arc-length from the start, in canvas pixels.
    pub s: f32,
    /// Normalized position along the curve, `0..=1` inside the curve.
    pub u: f32,
    pub pos: Point,
    /// Unit vector along the drawing direction.
    pub tangent: Point,
    /// Unit vector across the curve, already pointing at the generating side.
    pub normal: Point,
    pub pressure: f32,
    /// Turn rate, `1 / radius`.
    pub curvature: f32,
    /// How the body's surface is shaped here: positive where it bulges out (the
    /// generating side is inside the bend), negative where it is hollowed.
    /// Named for the surface rather than the line, since "convex" resolves
    /// either way depending on which of the two you have in mind.
    pub bulge: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpineField {
    samples: Vec<FrameSample>,
    length: f32,
    rest_length: f32,
    sign: f32,
}

impl SpineField {
    /// `sign` is `+1.0` for the left side, `-1.0` for the right. Pass a
    /// `rest_length` when re-mapping an edited curve so elements keep their
    /// place along it, `None` for a fresh stroke.
    #[must_use]
    pub fn new(nodes: &[SpineNode], sign: f32, rest_length: Option<f32>) -> Option<Self> {
        let dense = densify(nodes)?;
        let even = resample_even(&dense, SAMPLE_STEP);
        if even.len() < 2 {
            return None;
        }
        let sign = if sign < 0.0 { -1.0 } else { 1.0 };
        let samples = build_frames(&even, SAMPLE_STEP, sign);
        let length = samples.last().map_or(0.0, |f| f.s);
        if length <= f32::EPSILON {
            return None;
        }
        let rest_length = rest_length.filter(|r| *r > f32::EPSILON).unwrap_or(length);
        Some(Self {
            samples,
            length,
            rest_length,
            sign,
        })
    }

    /// Current arc length in canvas pixels.
    pub const fn length(&self) -> f32 {
        self.length
    }

    /// The length ribbon-space coordinates are expressed in: [`Self::length`]
    /// when the stroke was drawn, frozen there afterwards, so editing the curve
    /// stretches the pattern instead of reshuffling it.
    pub const fn rest_length(&self) -> f32 {
        self.rest_length
    }

    pub fn stretch(&self) -> f32 {
        if self.rest_length <= f32::EPSILON {
            1.0
        } else {
            self.length / self.rest_length
        }
    }

    pub const fn sign(&self) -> f32 {
        self.sign
    }

    pub fn samples(&self) -> &[FrameSample] {
        &self.samples
    }

    /// Positions outside the curve extrapolate along the end tangent, so an
    /// element rooted at the very tip still gets a usable frame.
    #[must_use]
    pub fn sample_at(&self, s: f32) -> FrameSample {
        let last = self.samples.len() - 1;
        if s <= 0.0 {
            let mut f = self.samples[0];
            f.pos = Point::new(f.pos.x + f.tangent.x * s, f.pos.y + f.tangent.y * s);
            f.s = s;
            f.u = s / self.length;
            return f;
        }
        if s >= self.length {
            let mut f = self.samples[last];
            let over = s - f.s;
            f.pos = Point::new(f.pos.x + f.tangent.x * over, f.pos.y + f.tangent.y * over);
            f.s = s;
            f.u = s / self.length;
            return f;
        }
        let pos = s / SAMPLE_STEP;
        let i = (pos.floor() as usize).min(last - 1);
        let t = pos - i as f32;
        lerp_frame(self.samples[i], self.samples[i + 1], t)
    }

    /// The frame at ribbon-space `s` (rest-length units).
    #[must_use]
    pub fn sample(&self, s: f32) -> FrameSample {
        self.sample_at(s * self.stretch())
    }

    #[must_use]
    pub fn map(&self, s: f32, n: f32) -> Point {
        let f = self.sample(s);
        Point::new(f.pos.x + f.normal.x * n, f.pos.y + f.normal.y * n)
    }

    /// Raw value of a modulation source at `frame`, in `0..=1`. `seed` only
    /// matters for [`ModSource::Noise`].
    #[must_use]
    pub fn source_value(&self, source: ModSource, frame: &FrameSample, seed: u64) -> f32 {
        match source {
            ModSource::Constant => 1.0,
            ModSource::Pressure => frame.pressure.clamp(0.0, 1.0),
            ModSource::Curvature => (frame.curvature * CURVATURE_REFERENCE).clamp(0.0, 1.0),
            ModSource::Noise => crate::noise::fbm_1d(seed, frame.s / NOISE_WAVELENGTH, 2),
            ModSource::Ends => {
                let d = frame.u.clamp(0.0, 1.0);
                crate::noise::smoothstep(d.min(1.0 - d) * 4.0)
            }
        }
    }

    #[must_use]
    pub fn modulate(
        &self,
        base: f32,
        modulation: Modulation,
        frame: &FrameSample,
        seed: u64,
    ) -> f32 {
        base * modulation.factor(self.source_value(modulation.source, frame, seed))
    }
}

/// Drop consecutive duplicates and interpolate into a dense polyline, carrying
/// pressure along.
fn densify(nodes: &[SpineNode]) -> Option<Vec<(Point, f32)>> {
    let mut clean: Vec<SpineNode> = Vec::with_capacity(nodes.len());
    for node in nodes {
        if clean
            .last()
            .is_some_and(|prev| prev.pos.distance(node.pos) < 0.01)
        {
            continue;
        }
        clean.push(*node);
    }
    if clean.len() < 2 {
        return None;
    }
    if clean.len() == 2 {
        return Some(vec![
            (clean[0].pos, clean[0].pressure),
            (clean[1].pos, clean[1].pressure),
        ]);
    }

    let mut out = Vec::with_capacity(clean.len() * 8);
    for i in 0..clean.len() - 1 {
        let p1 = clean[i];
        let p2 = clean[i + 1];
        let p0 = clean[i.saturating_sub(1)];
        let p3 = clean[(i + 2).min(clean.len() - 1)];
        let seg_len = p1.pos.distance(p2.pos);
        let steps = ((seg_len / SAMPLE_STEP).ceil() as usize).clamp(2, 128);
        for step in 0..steps {
            let t = step as f32 / steps as f32;
            out.push((
                catmull_rom(p0.pos, p1.pos, p2.pos, p3.pos, t),
                p1.pressure + (p2.pressure - p1.pressure) * t,
            ));
        }
    }
    let last = clean[clean.len() - 1];
    out.push((last.pos, last.pressure));
    Some(out)
}

/// Centripetal Catmull-Rom (alpha = 0.5): passes through every node without
/// the loops and overshoot the uniform form produces on sharp corners.
fn catmull_rom(p0: Point, p1: Point, p2: Point, p3: Point, t: f32) -> Point {
    let knot = |acc: f32, a: Point, b: Point| acc + a.distance(b).sqrt().max(1e-4);
    let t0 = 0.0;
    let t1 = knot(t0, p0, p1);
    let t2 = knot(t1, p1, p2);
    let t3 = knot(t2, p2, p3);
    let t = t1 + (t2 - t1) * t;

    let interp = |a: Point, b: Point, ta: f32, tb: f32| {
        let span = tb - ta;
        if span.abs() < 1e-6 {
            return a;
        }
        a.lerp(b, (t - ta) / span)
    };
    let a1 = interp(p0, p1, t0, t1);
    let a2 = interp(p1, p2, t1, t2);
    let a3 = interp(p2, p3, t2, t3);
    let b1 = interp(a1, a2, t0, t2);
    let b2 = interp(a2, a3, t1, t3);
    interp(b1, b2, t1, t2)
}

fn resample_even(dense: &[(Point, f32)], step: f32) -> Vec<(Point, f32)> {
    let total: f32 = dense
        .windows(2)
        .map(|w| w[0].0.distance(w[1].0))
        .sum();
    if total < step {
        let (Some(first), Some(last)) = (dense.first(), dense.last()) else {
            return Vec::new();
        };
        return vec![*first, *last];
    }
    // Whole steps only, so `s = i * step` holds exactly for every sample. The
    // tail short of a full step is dropped; at 2 px that is invisible.
    let count = (total / step).floor() as usize;
    let mut out = Vec::with_capacity(count + 1);
    let mut seg = 0_usize;
    let mut seg_start = 0.0_f32;
    for i in 0..=count {
        let target = (i as f32 * step).min(total);
        while seg + 1 < dense.len() - 1 {
            let seg_len = dense[seg].0.distance(dense[seg + 1].0);
            if seg_start + seg_len >= target {
                break;
            }
            seg_start += seg_len;
            seg += 1;
        }
        let seg_len = dense[seg].0.distance(dense[seg + 1].0);
        let t = if seg_len < f32::EPSILON {
            0.0
        } else {
            ((target - seg_start) / seg_len).clamp(0.0, 1.0)
        };
        let (a, pa) = dense[seg];
        let (b, pb) = dense[seg + 1];
        out.push((a.lerp(b, t), pa + (pb - pa) * t));
    }
    out
}

fn build_frames(even: &[(Point, f32)], step: f32, sign: f32) -> Vec<FrameSample> {
    let last = even.len() - 1;
    let total = last as f32 * step;
    let mut out = Vec::with_capacity(even.len());
    for (i, &(pos, pressure)) in even.iter().enumerate() {
        // Central difference in the middle, one-sided at the ends.
        let prev = even[i.saturating_sub(1)].0;
        let next = even[(i + 1).min(last)].0;
        let tangent = Point::new(next.x - prev.x, next.y - prev.y).normalize();
        let tangent = if tangent == Point::ZERO {
            Point::new(1.0, 0.0)
        } else {
            tangent
        };
        // Left of the direction of travel in screen space (y down).
        let normal = Point::new(tangent.y * sign, -tangent.x * sign);
        let s = i as f32 * step;
        out.push(FrameSample {
            s,
            u: if total > 0.0 { s / total } else { 0.0 },
            pos,
            tangent,
            normal,
            pressure: pressure.clamp(0.0, 1.0),
            curvature: 0.0,
            bulge: 0.0,
        });
    }
    for i in 1..last {
        let a = out[i].pos;
        let before = out[i - 1].pos;
        let after = out[i + 1].pos;
        let v1 = Point::new(a.x - before.x, a.y - before.y).normalize();
        let v2 = Point::new(after.x - a.x, after.y - a.y).normalize();
        let dot = v1.x.mul_add(v2.x, v1.y * v2.y).clamp(-1.0, 1.0);
        out[i].curvature = dot.acos() / step;
        // The tangent's derivative points at the centre of the bend, so a
        // normal pointing the same way means this side is on the inside of it.
        let toward_centre = Point::new(v2.x - v1.x, v2.y - v1.y);
        let facing = out[i]
            .normal
            .x
            .mul_add(toward_centre.x, out[i].normal.y * toward_centre.y);
        out[i].bulge = if facing > 0.0 {
            out[i].curvature
        } else {
            -out[i].curvature
        };
    }
    if last >= 2 {
        out[0].curvature = out[1].curvature;
        out[0].bulge = out[1].bulge;
        out[last].curvature = out[last - 1].curvature;
        out[last].bulge = out[last - 1].bulge;
    }
    out
}

fn lerp_frame(a: FrameSample, b: FrameSample, t: f32) -> FrameSample {
    let mix = |x: f32, y: f32| x + (y - x) * t;
    let tangent = Point::new(mix(a.tangent.x, b.tangent.x), mix(a.tangent.y, b.tangent.y))
        .normalize();
    let tangent = if tangent == Point::ZERO {
        a.tangent
    } else {
        tangent
    };
    FrameSample {
        s: mix(a.s, b.s),
        u: mix(a.u, b.u),
        pos: a.pos.lerp(b.pos, t),
        tangent,
        normal: Point::new(mix(a.normal.x, b.normal.x), mix(a.normal.y, b.normal.y)).normalize(),
        pressure: mix(a.pressure, b.pressure),
        curvature: mix(a.curvature, b.curvature),
        bulge: mix(a.bulge, b.bulge),
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn line(len: f32) -> Vec<SpineNode> {
        vec![
            SpineNode::new(Point::new(0.0, 0.0), 0.25),
            SpineNode::new(Point::new(len, 0.0), 1.0),
        ]
    }

    #[test]
    fn degenerate_curves_are_rejected() {
        assert!(SpineField::new(&[], 1.0, None).is_none());
        assert!(SpineField::new(&line(0.0)[..1], 1.0, None).is_none());
        let dot = vec![
            SpineNode::new(Point::new(5.0, 5.0), 1.0),
            SpineNode::new(Point::new(5.0, 5.0), 1.0),
        ];
        assert!(SpineField::new(&dot, 1.0, None).is_none());
    }

    #[test]
    fn straight_line_has_the_expected_length_and_frame() {
        let field = SpineField::new(&line(100.0), 1.0, None).expect("field");
        assert!((field.length() - 100.0).abs() < 1.0, "len {}", field.length());
        let mid = field.sample_at(50.0);
        assert!((mid.pos.x - 50.0).abs() < 0.5);
        assert!(mid.pos.y.abs() < 0.001);
        assert!((mid.tangent.x - 1.0).abs() < 0.001);
        // Left of a rightward stroke is up the screen (negative y).
        assert!((mid.normal.y + 1.0).abs() < 0.001, "normal {:?}", mid.normal);
        assert!(mid.curvature.abs() < 0.001, "a straight line does not bend");
    }

    #[test]
    fn side_flips_the_normal() {
        let left = SpineField::new(&line(100.0), 1.0, None).expect("field");
        let right = SpineField::new(&line(100.0), -1.0, None).expect("field");
        let a = left.sample_at(50.0).normal;
        let b = right.sample_at(50.0).normal;
        assert!((a.x + b.x).abs() < 1e-5 && (a.y + b.y).abs() < 1e-5);
    }

    #[test]
    fn pressure_is_interpolated_along_the_curve() {
        let field = SpineField::new(&line(100.0), 1.0, None).expect("field");
        assert!((field.sample_at(0.0).pressure - 0.25).abs() < 0.05);
        assert!((field.sample_at(100.0).pressure - 1.0).abs() < 0.05);
        let mid = field.sample_at(50.0).pressure;
        assert!((mid - 0.625).abs() < 0.05, "mid pressure {mid}");
    }

    #[test]
    fn normals_stay_perpendicular_on_a_curved_spine() {
        let nodes: Vec<SpineNode> = (0..12)
            .map(|i| {
                let t = i as f32 * 0.4;
                SpineNode::new(Point::new(t * 20.0, (t.sin()) * 40.0), 1.0)
            })
            .collect();
        let field = SpineField::new(&nodes, 1.0, None).expect("field");
        for f in field.samples() {
            let dot = f.tangent.x.mul_add(f.normal.x, f.tangent.y * f.normal.y);
            assert!(dot.abs() < 1e-4, "not perpendicular: {dot}");
            let len = f.normal.x.hypot(f.normal.y);
            assert!((len - 1.0).abs() < 1e-4, "normal not unit: {len}");
        }
    }

    #[test]
    fn curvature_reads_a_known_arc() {
        // Radius 100, so curvature should land near 1/100.
        let nodes: Vec<SpineNode> = (0..=40)
            .map(|i| {
                let a = (i as f32 / 40.0) * std::f32::consts::FRAC_PI_2;
                SpineNode::new(Point::new(a.cos() * 100.0, a.sin() * 100.0), 1.0)
            })
            .collect();
        let field = SpineField::new(&nodes, 1.0, None).expect("field");
        let mid = field.sample_at(field.length() * 0.5).curvature;
        assert!((mid - 0.01).abs() < 0.003, "curvature {mid}, expected ~0.01");
    }

    #[test]
    fn sampling_outside_the_curve_extrapolates() {
        let field = SpineField::new(&line(100.0), 1.0, None).expect("field");
        let before = field.sample_at(-20.0);
        assert!((before.pos.x + 20.0).abs() < 0.5, "x {}", before.pos.x);
        let after = field.sample_at(130.0);
        assert!((after.pos.x - 130.0).abs() < 1.0, "x {}", after.pos.x);
    }

    #[test]
    fn ribbon_space_follows_a_stretched_curve() {
        let field = SpineField::new(&line(200.0), 1.0, Some(100.0)).expect("field");
        assert!((field.stretch() - 2.0).abs() < 0.05);
        let end = field.map(100.0, 0.0);
        assert!((end.x - 200.0).abs() < 1.0, "x {}", end.x);
        let mid = field.map(50.0, 0.0);
        assert!((mid.x - 100.0).abs() < 1.0, "x {}", mid.x);
    }

    #[test]
    fn map_offsets_along_the_normal() {
        let field = SpineField::new(&line(100.0), 1.0, None).expect("field");
        let p = field.map(50.0, 10.0);
        assert!((p.x - 50.0).abs() < 0.5);
        assert!((p.y + 10.0).abs() < 0.1, "y {}", p.y);
    }

    #[test]
    fn ends_source_peaks_in_the_middle() {
        let field = SpineField::new(&line(100.0), 1.0, None).expect("field");
        let start = field.sample_at(0.0);
        let mid = field.sample_at(50.0);
        assert!(field.source_value(ModSource::Ends, &start, 0) < 0.05);
        assert!(field.source_value(ModSource::Ends, &mid, 0) > 0.95);
    }

    #[test]
    fn modulate_scales_by_pressure() {
        let field = SpineField::new(&line(100.0), 1.0, None).expect("field");
        let full = field.sample_at(100.0);
        let light = field.sample_at(0.0);
        let m = Modulation::new(ModSource::Pressure, 1.0);
        assert!((field.modulate(40.0, m, &full, 0) - 40.0).abs() < 0.5);
        assert!((field.modulate(40.0, m, &light, 0) - 10.0).abs() < 2.0);
    }

    #[test]
    fn side_signs_cover_both_passes() {
        assert_eq!(Side::Left.signs(), &[1.0]);
        assert_eq!(Side::Right.signs(), &[-1.0]);
        assert_eq!(Side::Both.signs().len(), 2);
    }
}
