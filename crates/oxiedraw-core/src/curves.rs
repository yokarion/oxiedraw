//! Photoshop-style tone curves (clamped natural splines on sRGB levels) for the
//! Curves filter and effect. Points live inline so `FilterSpec` stays `Copy`.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

pub const MAX_POINTS: usize = 16;

pub const LUT_SIZE: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "(u8, u8)", into = "(u8, u8)")]
pub struct CurvePoint {
    pub x: u8,
    pub y: u8,
}

impl CurvePoint {
    #[must_use]
    pub const fn new(x: u8, y: u8) -> Self {
        Self { x, y }
    }
}

impl From<(u8, u8)> for CurvePoint {
    fn from((x, y): (u8, u8)) -> Self {
        Self { x, y }
    }
}

impl From<CurvePoint> for (u8, u8) {
    fn from(p: CurvePoint) -> Self {
        (p.x, p.y)
    }
}

/// Control points sorted by strictly increasing `x`, at least two of them.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(from = "Vec<CurvePoint>", into = "Vec<CurvePoint>")]
pub struct Curve {
    len: u8,
    points: [CurvePoint; MAX_POINTS],
}

// Slots past `len` are stale leftovers, so only the live points take part.
impl PartialEq for Curve {
    fn eq(&self, other: &Self) -> bool {
        self.points() == other.points()
    }
}

impl Eq for Curve {}

impl std::fmt::Debug for Curve {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.points()).finish()
    }
}

impl Default for Curve {
    fn default() -> Self {
        Self::identity()
    }
}

impl From<Vec<CurvePoint>> for Curve {
    fn from(points: Vec<CurvePoint>) -> Self {
        Self::from_points(&points)
    }
}

impl From<Curve> for Vec<CurvePoint> {
    fn from(curve: Curve) -> Self {
        curve.points().to_vec()
    }
}

impl Curve {
    #[must_use]
    pub const fn identity() -> Self {
        let mut points = [CurvePoint::new(0, 0); MAX_POINTS];
        points[1] = CurvePoint::new(255, 255);
        Self { len: 2, points }
    }

    /// Sanitises hand-edited input: sorted, deduped by `x`, capped, at least two.
    #[must_use]
    pub fn from_points(points: &[CurvePoint]) -> Self {
        let mut sorted: Vec<CurvePoint> = points.to_vec();
        sorted.sort_by_key(|p| p.x);
        let mut unique: Vec<CurvePoint> = Vec::with_capacity(sorted.len());
        for p in sorted {
            match unique.last_mut() {
                Some(last) if last.x == p.x => *last = p,
                _ => unique.push(p),
            }
        }
        if unique.len() < 2 {
            return Self::identity();
        }
        unique.truncate(MAX_POINTS);
        let mut curve = Self {
            len: unique.len() as u8,
            points: [CurvePoint::default(); MAX_POINTS],
        };
        curve.points[..unique.len()].copy_from_slice(&unique);
        curve
    }

    #[must_use]
    pub fn points(&self) -> &[CurvePoint] {
        &self.points[..self.len as usize]
    }

    pub fn insert(&mut self, point: CurvePoint) -> Option<usize> {
        let len = self.len as usize;
        let at = self.points().partition_point(|p| p.x < point.x);
        if self.points().get(at).is_some_and(|p| p.x == point.x) {
            self.points[at] = point;
            return Some(at);
        }
        if len >= MAX_POINTS {
            return None;
        }
        self.points.copy_within(at..len, at + 1);
        self.points[at] = point;
        self.len += 1;
        Some(at)
    }

    pub fn remove(&mut self, index: usize) -> bool {
        let len = self.len as usize;
        if len <= 2 || index >= len {
            return false;
        }
        self.points.copy_within(index + 1..len, index);
        self.len -= 1;
        true
    }

    pub fn move_point(&mut self, index: usize, point: CurvePoint) -> Option<CurvePoint> {
        let pts = self.points();
        if index >= pts.len() {
            return None;
        }
        let min_x = index.checked_sub(1).map_or(0, |i| pts[i].x.saturating_add(1));
        let max_x = pts.get(index + 1).map_or(255, |p| p.x.saturating_sub(1));
        let moved = CurvePoint::new(point.x.max(min_x).min(max_x), point.y);
        self.points[index] = moved;
        Some(moved)
    }

    #[must_use]
    pub fn is_identity(&self) -> bool {
        let pts = self.points();
        pts.iter().all(|p| p.x == p.y) && pts[0].x == 0 && pts[pts.len() - 1].x == 255
    }

    #[must_use]
    pub fn evaluate(&self, x: f32) -> f32 {
        Spline::new(self.points()).eval(x)
    }

    pub fn sample_into(&self, out: &mut [f32]) {
        let spline = Spline::new(self.points());
        let last = out.len().saturating_sub(1).max(1) as f32;
        for (i, v) in out.iter_mut().enumerate() {
            *v = spline.eval(i as f32 / last);
        }
    }

    #[must_use]
    pub fn bake(&self) -> [f32; LUT_SIZE] {
        let mut lut = [0.0; LUT_SIZE];
        self.sample_into(&mut lut);
        lut
    }
}

#[must_use]
pub fn sample_baked(lut: &[f32; LUT_SIZE], x: f32) -> f32 {
    let pos = x.clamp(0.0, 1.0) * (LUT_SIZE - 1) as f32;
    let lo = (pos as usize).min(LUT_SIZE - 2);
    let t = pos - lo as f32;
    lut[lo] + (lut[lo + 1] - lut[lo]) * t
}

struct Spline {
    xs: [f32; MAX_POINTS],
    ys: [f32; MAX_POINTS],
    second_derivatives: [f32; MAX_POINTS],
    len: usize,
}

impl Spline {
    fn new(points: &[CurvePoint]) -> Self {
        let len = points.len().min(MAX_POINTS);
        let mut xs = [0.0; MAX_POINTS];
        let mut ys = [0.0; MAX_POINTS];
        for (i, p) in points.iter().take(len).enumerate() {
            xs[i] = f32::from(p.x) / 255.0;
            ys[i] = f32::from(p.y) / 255.0;
        }
        let mut d2 = [0.0; MAX_POINTS];
        let mut u = [0.0; MAX_POINTS];
        for i in 1..len.saturating_sub(1) {
            let sig = (xs[i] - xs[i - 1]) / (xs[i + 1] - xs[i - 1]);
            let p = sig * d2[i - 1] + 2.0;
            d2[i] = (sig - 1.0) / p;
            let slope_diff = (ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i])
                - (ys[i] - ys[i - 1]) / (xs[i] - xs[i - 1]);
            u[i] = (6.0 * slope_diff / (xs[i + 1] - xs[i - 1]) - sig * u[i - 1]) / p;
        }
        for k in (0..len.saturating_sub(1)).rev() {
            d2[k] = d2[k] * d2[k + 1] + u[k];
        }
        Self {
            xs,
            ys,
            second_derivatives: d2,
            len,
        }
    }

    fn eval(&self, x: f32) -> f32 {
        let n = self.len;
        if n == 0 {
            return x.clamp(0.0, 1.0);
        }
        if x <= self.xs[0] {
            return self.ys[0];
        }
        if x >= self.xs[n - 1] {
            return self.ys[n - 1];
        }
        let k = self.xs[..n].partition_point(|&px| px <= x).saturating_sub(1);
        let (x0, x1) = (self.xs[k], self.xs[k + 1]);
        let h = x1 - x0;
        let a = (x1 - x) / h;
        let b = (x - x0) / h;
        let d2 = &self.second_derivatives;
        let y = a * self.ys[k]
            + b * self.ys[k + 1]
            + ((a * a * a - a) * d2[k] + (b * b * b - b) * d2[k + 1]) * h * h / 6.0;
        y.clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CurveChannel {
    #[default]
    Rgb,
    Red,
    Green,
    Blue,
}

impl crate::enum_meta::EnumMeta for CurveChannel {
    const ALL: &'static [Self] = &[Self::Rgb, Self::Red, Self::Green, Self::Blue];

    fn label(self) -> &'static str {
        match self {
            Self::Rgb => "RGB",
            Self::Red => "R",
            Self::Green => "G",
            Self::Blue => "B",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CurveSet {
    #[serde(default)]
    pub rgb: Curve,
    #[serde(default)]
    pub red: Curve,
    #[serde(default)]
    pub green: Curve,
    #[serde(default)]
    pub blue: Curve,
}

impl CurveSet {
    #[must_use]
    pub const fn curve(&self, channel: CurveChannel) -> &Curve {
        match channel {
            CurveChannel::Rgb => &self.rgb,
            CurveChannel::Red => &self.red,
            CurveChannel::Green => &self.green,
            CurveChannel::Blue => &self.blue,
        }
    }

    pub const fn curve_mut(&mut self, channel: CurveChannel) -> &mut Curve {
        match channel {
            CurveChannel::Rgb => &mut self.rgb,
            CurveChannel::Red => &mut self.red,
            CurveChannel::Green => &mut self.green,
            CurveChannel::Blue => &mut self.blue,
        }
    }

    #[must_use]
    pub fn is_identity(&self) -> bool {
        [self.rgb, self.red, self.green, self.blue]
            .iter()
            .all(Curve::is_identity)
    }

    /// R/G/B the channel curves, A the master: the layout the shader reads.
    #[must_use]
    pub fn bake_lut(&self) -> Vec<f32> {
        let channels = [
            self.red.bake(),
            self.green.bake(),
            self.blue.bake(),
            self.rgb.bake(),
        ];
        (0..LUT_SIZE)
            .flat_map(|i| channels.iter().map(move |c| c[i]))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Histogram {
    bins: [[u32; LUT_SIZE]; 4],
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            bins: [[0; LUT_SIZE]; 4],
        }
    }
}

impl Histogram {
    #[must_use]
    pub fn from_bgra(pixels: &[u8], mask: Option<&[u8]>) -> Self {
        let mut histogram = Self::default();
        histogram.add_bgra(pixels, mask);
        histogram
    }

    pub fn add_bgra(&mut self, pixels: &[u8], mask: Option<&[u8]>) {
        let levels = unpremultiplied_levels();
        for (i, px) in pixels.chunks_exact(4).enumerate() {
            let alpha = usize::from(px[3]);
            if alpha == 0 || mask.is_some_and(|m| m.get(i).is_none_or(|&v| v == 0)) {
                continue;
            }
            let level = |c: u8| usize::from(levels[(alpha << 8) | usize::from(c)]);
            let (b, g, r) = (level(px[0]), level(px[1]), level(px[2]));
            self.bins[bin(CurveChannel::Red)][r] += 1;
            self.bins[bin(CurveChannel::Green)][g] += 1;
            self.bins[bin(CurveChannel::Blue)][b] += 1;
            let rgb = &mut self.bins[bin(CurveChannel::Rgb)];
            rgb[r] += 1;
            rgb[g] += 1;
            rgb[b] += 1;
        }
    }

    #[must_use]
    pub const fn channel(&self, channel: CurveChannel) -> &[u32; LUT_SIZE] {
        &self.bins[bin(channel)]
    }
}

const fn bin(channel: CurveChannel) -> usize {
    match channel {
        CurveChannel::Rgb => 0,
        CurveChannel::Red => 1,
        CurveChannel::Green => 2,
        CurveChannel::Blue => 3,
    }
}

/// sRGB level per `alpha << 8 | byte`; stored bytes are premultiplied linear.
fn unpremultiplied_levels() -> &'static [u8] {
    static LEVELS: OnceLock<Vec<u8>> = OnceLock::new();
    LEVELS.get_or_init(|| {
        let mut levels = vec![0; 256 * 256];
        for alpha in 1..256 {
            for byte in 0..256 {
                let linear = oxiedraw_utils::color::srgb_to_linear(byte as u8);
                let straight = linear * 255.0 / alpha as f32;
                levels[(alpha << 8) | byte] = oxiedraw_utils::color::linear_to_srgb(straight);
            }
        }
        levels
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    fn curve(points: &[(u8, u8)]) -> Curve {
        let pts: Vec<CurvePoint> = points.iter().map(|&p| p.into()).collect();
        Curve::from_points(&pts)
    }

    #[test]
    fn identity_bakes_to_a_linear_ramp() {
        let lut = Curve::identity().bake();
        for (i, v) in lut.iter().enumerate() {
            assert!((v - i as f32 / 255.0).abs() < 1e-5, "level {i} -> {v}");
        }
        assert!(CurveSet::default().is_identity());
    }

    #[test]
    fn spline_passes_through_its_points() {
        let c = curve(&[(0, 0), (64, 128), (192, 96), (255, 255)]);
        for p in c.points() {
            let y = c.evaluate(f32::from(p.x) / 255.0);
            assert!((y - f32::from(p.y) / 255.0).abs() < 1e-4, "{p:?} -> {y}");
        }
    }

    #[test]
    fn curve_is_flat_outside_its_endpoints() {
        let c = curve(&[(40, 30), (200, 220)]);
        assert_eq!(c.evaluate(0.0), 30.0 / 255.0);
        assert_eq!(c.evaluate(1.0), 220.0 / 255.0);
    }

    #[test]
    fn overshoot_is_clamped() {
        let c =curve(&[(0, 0), (64, 255), (128, 255), (255, 255)]);
        assert_eq!(c.evaluate(96.0 / 255.0), 1.0);
        let wild = curve(&[(0, 0), (40, 255), (80, 0), (255, 0)]);
        for v in wild.bake() {
            assert!((0.0..=1.0).contains(&v));
        }
    }

    #[test]
    fn collinear_points_stay_identity() {
        let c = curve(&[(0, 0), (100, 100), (255, 255)]);
        assert!(c.is_identity());
        for (i, v) in c.bake().iter().enumerate() {
            assert!((v - i as f32 / 255.0).abs() < 1e-4);
        }
        assert!(!curve(&[(10, 10), (255, 255)]).is_identity());
    }

    #[test]
    fn from_points_sorts_dedups_and_caps() {
        let c = curve(&[(200, 1), (10, 2), (200, 3)]);
        assert_eq!(c.points(), &[CurvePoint::new(10, 2), CurvePoint::new(200, 3)]);

        let many: Vec<(u8, u8)> = (0..40).map(|i| (i * 6, i)).collect();
        assert_eq!(curve(&many).points().len(), MAX_POINTS);

        assert_eq!(curve(&[(5, 5)]), Curve::identity());
    }

    #[test]
    fn insert_keeps_order_and_replaces_same_x() {
        let mut c = Curve::identity();
        assert_eq!(c.insert(CurvePoint::new(100, 40)), Some(1));
        assert_eq!(c.insert(CurvePoint::new(50, 10)), Some(1));
        assert_eq!(c.insert(CurvePoint::new(100, 90)), Some(2));
        let xs: Vec<u8> = c.points().iter().map(|p| p.x).collect();
        assert_eq!(xs, [0, 50, 100, 255]);
        assert_eq!(c.points()[2].y, 90);

        for x in 1..=40 {
            c.insert(CurvePoint::new(x * 2 + 101, 0));
        }
        assert_eq!(c.points().len(), MAX_POINTS);
        assert_eq!(c.insert(CurvePoint::new(1, 1)), None, "full curve");
    }

    #[test]
    fn remove_keeps_two_points() {
        let mut c = curve(&[(0, 0), (128, 60), (255, 255)]);
        assert!(!c.remove(3));
        assert!(c.remove(1));
        assert_eq!(c, Curve::identity());
        assert!(!c.remove(0), "two points is the minimum");
    }

    #[test]
    fn move_point_stays_between_neighbours() {
        let mut c = curve(&[(0, 0), (100, 100), (150, 150), (255, 255)]);
        assert_eq!(c.move_point(1, CurvePoint::new(200, 7)), Some(CurvePoint::new(149, 7)));
        assert_eq!(c.move_point(2, CurvePoint::new(0, 7)), Some(CurvePoint::new(150, 7)));
        assert_eq!(c.move_point(0, CurvePoint::new(255, 30)), Some(CurvePoint::new(148, 30)));
        assert_eq!(c.move_point(3, CurvePoint::new(0, 30)), Some(CurvePoint::new(151, 30)));
        assert_eq!(c.move_point(4, CurvePoint::new(0, 0)), None);
        let xs: Vec<u8> = c.points().iter().map(|p| p.x).collect();
        assert_eq!(xs, [148, 149, 150, 151]);
    }

    #[test]
    fn curve_set_round_trips_as_point_pairs() {
        let set = CurveSet {
            red: curve(&[(0, 20), (128, 200), (255, 255)]),
            ..CurveSet::default()
        };
        let json = serde_json::to_string(&set).unwrap();
        assert!(json.contains("\"red\":[[0,20],[128,200],[255,255]]"), "{json}");
        let back: CurveSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back, set);

        let partial: CurveSet = serde_json::from_str(r#"{"blue":[[0,255],[255,0]]}"#).unwrap();
        assert!(partial.rgb.is_identity());
        assert_eq!(partial.blue.evaluate(0.0), 1.0);
    }

    #[test]
    fn lut_layout_is_channels_then_master() {
        let set = CurveSet {
            green: curve(&[(0, 255), (255, 0)]),
            rgb: curve(&[(0, 51), (255, 51)]),
            ..CurveSet::default()
        };
        let lut = set.bake_lut();
        assert_eq!(lut.len(), LUT_SIZE * 4);
        assert_eq!(lut[0], 0.0);
        assert_eq!(lut[1], 1.0);
        assert_eq!(lut[3], 0.2);
        assert!((lut[255 * 4] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn histogram_counts_levels_and_skips_masked_and_clear_pixels() {
        let pixels = [10, 20, 30, 255, 0, 0, 0, 0, 10, 20, 30, 255];
        let h = Histogram::from_bgra(&pixels, Some(&[255, 255, 0]));
        assert_eq!(h.channel(CurveChannel::Red)[30], 1);
        assert_eq!(h.channel(CurveChannel::Green)[20], 1);
        assert_eq!(h.channel(CurveChannel::Blue)[10], 1);
        assert_eq!(h.channel(CurveChannel::Rgb).iter().sum::<u32>(), 3);
        assert_eq!(h.channel(CurveChannel::Red)[0], 0);
    }

    #[test]
    fn opaque_bytes_are_their_own_level() {
        let levels = unpremultiplied_levels();
        for byte in 0..256 {
            assert_eq!(usize::from(levels[(255 << 8) | byte]), byte);
        }
    }

    #[test]
    fn sample_baked_blends_between_levels() {
        let lut = curve(&[(0, 0), (255, 51)]).bake();
        assert_eq!(sample_baked(&lut, 0.0), lut[0]);
        assert_eq!(sample_baked(&lut, 1.0), lut[255]);
        let mid = sample_baked(&lut, 10.5 / 255.0);
        assert!((mid - f32::midpoint(lut[10], lut[11])).abs() < 1e-6);
    }

    #[test]
    fn histogram_unpremultiplies_translucent_pixels() {
        let byte = oxiedraw_utils::color::linear_to_srgb(0.5 * 1.0);
        let h = Histogram::from_bgra(&[byte, byte, byte, 128], None);
        let white = h.channel(CurveChannel::Red)[254..].iter().sum::<u32>();
        assert_eq!(white, 1, "translucent white should count as ~255");
    }
}
