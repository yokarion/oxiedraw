//! Drawing guides: symmetry, 2D grid, isometric, and perspective overlays.
//!
//! A [`GuideConfig`] is per-document state (persisted, schema v10). The
//! Drawing Guide tool edits it live; once "Assisted Drawing" is on the guide
//! keeps affecting brush strokes even after the tool is left, mirroring
//! Procreate's committed guides.
//!
//! The symmetry assist is implemented as *dab expansion*: [`symmetry_dabs`]
//! turns each painted point into its mirrored/rotated copies, so the stroke is
//! reproduced in real time at the GPU dab level with no extra brush state.

use std::cell::RefCell;
use std::f32::consts::{FRAC_PI_2, FRAC_PI_3, FRAC_PI_4, PI};
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::enum_meta::EnumMeta;
use oxiedraw_utils::geometry::Point;

/// Which family of guide is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GuideKind {
    #[default]
    Symmetry,
    Grid2D,
    Isometric,
    Perspective,
}

impl EnumMeta for GuideKind {
    const ALL: &'static [Self] =
        &[Self::Symmetry, Self::Grid2D, Self::Isometric, Self::Perspective];

    fn label(self) -> &'static str {
        match self {
            Self::Symmetry => "Symmetry",
            Self::Grid2D => "2D Grid",
            Self::Isometric => "Isometric",
            Self::Perspective => "Perspective",
        }
    }
}

/// Symmetry axis layout. Drives both the drawn guide lines and the set of
/// reproduction transforms. `Axis` is a single mirror line (Procreate's
/// Vertical and Horizontal are the same operation at different angles, so they
/// are merged into one rotatable axis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SymmetryMode {
    #[default]
    Axis,
    Quadrant,
    Radial,
}

impl EnumMeta for SymmetryMode {
    const ALL: &'static [Self] = &[Self::Axis, Self::Quadrant, Self::Radial];

    fn label(self) -> &'static str {
        match self {
            Self::Axis => "Axis",
            Self::Quadrant => "Quadrant",
            Self::Radial => "Radial",
        }
    }
}

/// One symmetry group element applied about the guide origin. The identity
/// (the real dab) is always painted separately, so this only enumerates the
/// extra copies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SymElement {
    /// Rotate the copy by `angle` radians about the origin.
    Rotate { angle: f32 },
    /// Reflect the copy across the line through the origin at `axis` radians.
    Reflect { axis: f32 },
}

impl SymElement {
    /// Map a painted point (and its dab orientation) to this copy. Returns the
    /// transformed centre, the copy's rotation, and whether handedness flipped
    /// (reflections flip - used to mirror asymmetric tips).
    #[must_use]
    pub fn apply(self, origin: Point, center: Point, rotation: f32) -> (Point, f32, bool) {
        let dx = center.x - origin.x;
        let dy = center.y - origin.y;
        match self {
            Self::Rotate { angle } => {
                let (s, c) = angle.sin_cos();
                let nx = c * dx - s * dy;
                let ny = s * dx + c * dy;
                (
                    Point::new(origin.x + nx, origin.y + ny),
                    rotation + angle,
                    false,
                )
            }
            Self::Reflect { axis } => {
                // Reflect the offset across a line at `axis`: rotate into the
                // line frame, flip Y, rotate back. Angle maps r -> 2*axis - r.
                let (s, c) = (2.0 * axis).sin_cos();
                let nx = c * dx + s * dy;
                let ny = s * dx - c * dy;
                (
                    Point::new(origin.x + nx, origin.y + ny),
                    2.0 * axis - rotation,
                    true,
                )
            }
        }
    }
}

/// Resolved symmetry transform set for a live stroke, handed to the renderer
/// stamp path so each painted dab is reproduced across every copy.
#[derive(Debug, Clone)]
pub struct Symmetry {
    pub origin: Point,
    pub elements: Vec<SymElement>,
}

impl Symmetry {
    /// Build from a guide config, or `None` if it doesn't reproduce strokes.
    #[must_use]
    pub fn from_config(cfg: &GuideConfig) -> Option<Self> {
        if !cfg.reproduces_strokes() {
            return None;
        }
        let elements = symmetry_elements(cfg.symmetry, cfg.rotational, cfg.angle);
        if elements.is_empty() {
            return None;
        }
        Some(Self { origin: cfg.origin, elements })
    }
}

/// The set of extra copies a symmetry mode produces. `angle` is the guide's
/// primary axis orientation (radians). Mirror mode uses reflections; rotational
/// mode replaces them with pure rotations of the same order.
#[must_use]
pub fn symmetry_elements(mode: SymmetryMode, rotational: bool, angle: f32) -> Vec<SymElement> {
    match mode {
        SymmetryMode::Axis => {
            if rotational {
                vec![SymElement::Rotate { angle: PI }]
            } else {
                vec![SymElement::Reflect { axis: angle }]
            }
        }
        SymmetryMode::Quadrant => {
            if rotational {
                vec![
                    SymElement::Rotate { angle: FRAC_PI_2 },
                    SymElement::Rotate { angle: PI },
                    SymElement::Rotate { angle: PI + FRAC_PI_2 },
                ]
            } else {
                vec![
                    SymElement::Reflect { axis: angle },
                    SymElement::Reflect { axis: angle + FRAC_PI_2 },
                    SymElement::Rotate { angle: PI },
                ]
            }
        }
        SymmetryMode::Radial => {
            if rotational {
                (1..8)
                    .map(|k| SymElement::Rotate { angle: k as f32 * FRAC_PI_4 })
                    .collect()
            } else {
                // Dihedral D4: 3 rotations + 4 reflections (8 segments total
                // with the identity).
                vec![
                    SymElement::Rotate { angle: FRAC_PI_2 },
                    SymElement::Rotate { angle: PI },
                    SymElement::Rotate { angle: PI + FRAC_PI_2 },
                    SymElement::Reflect { axis: angle },
                    SymElement::Reflect { axis: angle + FRAC_PI_4 },
                    SymElement::Reflect { axis: angle + FRAC_PI_2 },
                    SymElement::Reflect { axis: angle + FRAC_PI_4 + FRAC_PI_2 },
                ]
            }
        }
    }
}

/// Orientations (radians) of the guide lines drawn through the origin for a
/// symmetry mode. Used by the canvas overlay.
#[must_use]
pub fn symmetry_line_angles(mode: SymmetryMode, angle: f32) -> Vec<f32> {
    match mode {
        SymmetryMode::Axis => vec![angle],
        SymmetryMode::Quadrant => vec![angle, angle + FRAC_PI_2],
        SymmetryMode::Radial => vec![
            angle,
            angle + FRAC_PI_4,
            angle + FRAC_PI_2,
            angle + FRAC_PI_4 + FRAC_PI_2,
        ],
    }
}

/// Serde adapter for [`Point`] (which lives in the utils crate and has no
/// serde derive). Stores it as a plain `[x, y]` pair.
mod point_serde {
    use super::Point;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[allow(clippy::trivially_copy_pass_by_ref)] // serde `with` requires &T
    pub(super) fn serialize<S: Serializer>(p: &Point, s: S) -> Result<S::Ok, S::Error> {
        [p.x, p.y].serialize(s)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Point, D::Error> {
        let [x, y] = <[f32; 2]>::deserialize(d)?;
        Ok(Point::new(x, y))
    }
}

/// A perspective vanishing point in canvas-space coordinates, with its own
/// line colour (a position along the guide colour ramp) so each point's rays
/// are drawn in a distinct hue.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VanishingPoint {
    pub x: f32,
    pub y: f32,
    /// Ray colour as a position `0.0..=1.0` along the guide ramp. Absent in
    /// pre-v11 files (defaults to the ramp's blue).
    #[serde(default = "default_guide_color")]
    pub color: f32,
}

impl VanishingPoint {
    #[must_use]
    pub const fn new(x: f32, y: f32, color: f32) -> Self {
        Self { x, y, color }
    }

    #[must_use]
    pub const fn point(self) -> Point {
        Point::new(self.x, self.y)
    }
}

/// Full per-document guide configuration. Serialized into `document.json` as
/// the optional `guide` field (schema v10; absent = no guide).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuideConfig {
    pub kind: GuideKind,
    pub symmetry: SymmetryMode,
    /// Mirror (false) vs rotational (true) symmetry.
    pub rotational: bool,
    /// Drawing Assist: symmetry reproduces strokes; grid/perspective snap them.
    pub assisted: bool,
    /// Guide origin (position node) in canvas pixels.
    #[serde(with = "point_serde")]
    pub origin: Point,
    /// Primary axis orientation in radians (rotation node).
    pub angle: f32,
    /// Line opacity, `0.0..=1.0`.
    pub opacity: f32,
    /// Line thickness in canvas pixels.
    pub thickness: f32,
    /// Cell size for the 2D grid / isometric guides, in canvas pixels.
    pub grid_spacing: f32,
    /// Number of lines fanned from the vanishing point in perspective mode.
    #[serde(default = "default_perspective_rays")]
    pub perspective_rays: u32,
    /// Perspective vanishing points (1 to 3). Empty for other kinds.
    #[serde(default)]
    pub vanishing_points: Vec<VanishingPoint>,
    /// Line colour as a position `0.0..=1.0` along the guide colour ramp
    /// (see [`guide_line_color`]). Stored as the slider position so the ramp
    /// stays the single source of truth. Absent in pre-v11 files.
    #[serde(default = "default_guide_color")]
    pub color: f32,
}

fn default_perspective_rays() -> u32 {
    12
}

fn default_guide_color() -> f32 {
    // A blue near the start of the ramp, like Procreate's default guides.
    0.13
}

/// Resolve a guide line colour from its ramp position `t` (`0.0..=1.0`).
/// The ramp is a fixed, non-cyclic sweep: black -> blue -> cyan -> green ->
/// yellow -> orange -> red -> purple -> pink -> white (10 evenly spaced stops).
#[must_use]
pub fn guide_line_color(t: f32) -> (f32, f32, f32) {
    const STOPS: [(f32, f32, f32); 10] = [
        (0.0, 0.0, 0.0),   // black
        (0.0, 0.0, 1.0),   // blue
        (0.0, 1.0, 1.0),   // cyan
        (0.0, 1.0, 0.0),   // green
        (1.0, 1.0, 0.0),   // yellow
        (1.0, 0.5, 0.0),   // orange
        (1.0, 0.0, 0.0),   // red
        (0.5, 0.0, 1.0),   // purple
        (1.0, 0.4, 0.8),   // pink
        (1.0, 1.0, 1.0),   // white
    ];
    let segments = (STOPS.len() - 1) as f32;
    let scaled = t.clamp(0.0, 1.0) * segments;
    let i = (scaled.floor() as usize).min(STOPS.len() - 2);
    let f = scaled - i as f32;
    let (r0, g0, b0) = STOPS[i];
    let (r1, g1, b1) = STOPS[i + 1];
    (
        r0 + (r1 - r0) * f,
        g0 + (g1 - g0) * f,
        b0 + (b1 - b0) * f,
    )
}

/// Nearest ramp position (`0.0..=1.0`) to an arbitrary RGB colour (channels
/// `0.0..=1.0`). Used to seed a guide's colour from the theme accent or a VP's
/// from the primary colour - the ramp is coarse, so this only *approximately*
/// matches, which is the intent ("barely match").
#[must_use]
pub fn guide_pos_from_rgb(r: f32, g: f32, b: f32) -> f32 {
    const SAMPLES: u32 = 128;
    let mut best_t = 0.0;
    let mut best_d = f32::INFINITY;
    for i in 0..=SAMPLES {
        let t = i as f32 / SAMPLES as f32;
        let (cr, cg, cb) = guide_line_color(t);
        let d = (cr - r).powi(2) + (cg - g).powi(2) + (cb - b).powi(2);
        if d < best_d {
            best_d = d;
            best_t = t;
        }
    }
    best_t
}

/// Default ramp colour for vanishing point `index`: the primary colour for the
/// first point, then a 60-degree hue step per subsequent point, each snapped to
/// the ramp. Saturation/value are floored so a dull or near-gray primary still
/// lands on a visible ramp hue.
#[must_use]
pub fn vp_default_color(index: usize, primary: (f32, f32, f32)) -> f32 {
    let to_u8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    let (h, s, v) = oxiedraw_utils::color::rgb_to_hsv(
        to_u8(primary.0),
        to_u8(primary.1),
        to_u8(primary.2),
    );
    // Hue is normalized to `0.0..1.0`, so 60 degrees is 1/6 of a turn.
    let h = h + index as f32 / 6.0;
    let [r, g, b] = oxiedraw_utils::color::hsv_to_rgb(h, s.max(0.55), v.max(0.7));
    guide_pos_from_rgb(f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0)
}

impl GuideConfig {
    /// A fresh guide centred on a `width` x `height` canvas.
    #[must_use]
    pub fn centered(width: u32, height: u32) -> Self {
        let cx = width as f32 * 0.5;
        let cy = height as f32 * 0.5;
        Self {
            kind: GuideKind::Symmetry,
            symmetry: SymmetryMode::Axis,
            rotational: false,
            assisted: true,
            origin: Point::new(cx, cy),
            // A vertical mirror line (axis pointing straight up).
            angle: FRAC_PI_2,
            opacity: 0.9,
            thickness: 2.0,
            grid_spacing: 64.0,
            perspective_rays: default_perspective_rays(),
            vanishing_points: Vec::new(),
            color: default_guide_color(),
        }
    }

    /// Reset only position and rotation to the canvas centre / default axis,
    /// preserving mode (matches Procreate's node Reset).
    pub fn reset_position(&mut self, width: u32, height: u32) {
        self.origin = Point::new(width as f32 * 0.5, height as f32 * 0.5);
        self.angle = FRAC_PI_2;
    }

    /// True when this guide reproduces (not just snaps) brush strokes.
    #[must_use]
    pub fn reproduces_strokes(&self) -> bool {
        self.assisted && self.kind == GuideKind::Symmetry
    }

    /// True when this guide snaps a live stroke onto its lines (grid / isometric
    /// / perspective). Symmetry reproduces instead, so it never snaps.
    #[must_use]
    pub fn snaps_strokes(&self) -> bool {
        self.assisted
            && matches!(
                self.kind,
                GuideKind::Grid2D | GuideKind::Isometric | GuideKind::Perspective
            )
    }
}

/// A stroke locked onto one guide line: the anchor point plus the line's unit
/// direction. Every incoming point is projected onto this line so the stroke
/// runs perfectly straight along the guide, converging to a vanishing point in
/// perspective mode (the line toward a fixed VP is itself straight).
#[derive(Debug, Clone, Copy)]
pub struct AssistLock {
    pub start: Point,
    pub dir: Point,
}

impl AssistLock {
    /// Foot of the perpendicular from `p` onto the locked line.
    #[must_use]
    pub fn project(&self, p: Point) -> Point {
        let vx = p.x - self.start.x;
        let vy = p.y - self.start.y;
        let t = vx * self.dir.x + vy * self.dir.y;
        Point::new(self.start.x + self.dir.x * t, self.start.y + self.dir.y * t)
    }
}

/// Candidate snap-line directions (unit vectors, canvas space) for a stroke
/// starting at `start`. Grid/isometric give fixed axes; perspective gives the
/// direction toward each vanishing point plus horizontal/vertical (Procreate
/// also snaps horizon and upright lines). Empty when the guide doesn't snap.
#[must_use]
pub fn assist_candidates(cfg: &GuideConfig, start: Point) -> Vec<Point> {
    let unit = |a: f32| Point::new(a.cos(), a.sin());
    match cfg.kind {
        GuideKind::Symmetry => Vec::new(),
        GuideKind::Grid2D => vec![unit(0.0), unit(FRAC_PI_2)],
        GuideKind::Isometric => {
            let b = cfg.angle;
            vec![unit(b), unit(b + FRAC_PI_3), unit(b - FRAC_PI_3)]
        }
        GuideKind::Perspective => {
            let mut dirs: Vec<Point> = cfg
                .vanishing_points
                .iter()
                .filter_map(|vp| {
                    let d = Point::new(vp.x - start.x, vp.y - start.y);
                    (d.x.hypot(d.y) > 1e-3).then(|| d.normalize())
                })
                .collect();
            dirs.push(unit(0.0));
            dirs.push(unit(FRAC_PI_2));
            dirs
        }
    }
}

/// Pick the guide line that best matches the drag from `start` toward `toward`
/// and lock the stroke to it. A line and its reverse are equivalent, so the
/// candidate with the largest absolute alignment wins. `None` if the guide
/// doesn't snap or the pointer hasn't moved.
#[must_use]
pub fn assist_lock(cfg: &GuideConfig, start: Point, toward: Point) -> Option<AssistLock> {
    let cands = assist_candidates(cfg, start);
    if cands.is_empty() {
        return None;
    }
    let drag = Point::new(toward.x - start.x, toward.y - start.y);
    let len = drag.x.hypot(drag.y);
    if len < 1e-3 {
        return None;
    }
    let (ux, uy) = (drag.x / len, drag.y / len);
    let best = cands.into_iter().max_by(|a, b| {
        let da = (a.x * ux + a.y * uy).abs();
        let db = (b.x * ux + b.y * uy).abs();
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    })?;
    Some(AssistLock { start, dir: best })
}

/// Live, mutable guide state shared across the UI, plus change subscribers.
/// Backed by `Rc<RefCell>` like [`crate::tools::CropState`]. Holds an entry
/// snapshot so the tool's Cancel can restore the pre-edit config.
#[derive(Clone)]
pub struct GuideState {
    pub config: Rc<RefCell<Option<GuideConfig>>>,
    /// Snapshot taken when the Drawing Guide tool is entered, for Cancel.
    pub entry_snapshot: Rc<RefCell<Option<GuideConfig>>>,
    changed: Rc<RefCell<Vec<Box<dyn Fn()>>>>,
}

impl GuideState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: Rc::new(RefCell::new(None)),
            entry_snapshot: Rc::new(RefCell::new(None)),
            changed: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub fn notify_changed(&self) {
        for cb in self.changed.borrow().iter() {
            cb();
        }
    }

    pub fn connect_changed(&self, cb: Box<dyn Fn()>) {
        self.changed.borrow_mut().push(cb);
    }

    /// Mutate the config in place (creating nothing if absent) and notify.
    pub fn update(&self, f: impl FnOnce(&mut GuideConfig)) {
        if let Some(cfg) = self.config.borrow_mut().as_mut() {
            f(cfg);
        }
        self.notify_changed();
    }
}

impl Default for GuideState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: Point, b: Point) {
        assert!((a.x - b.x).abs() < 1e-3 && (a.y - b.y).abs() < 1e-3, "{a:?} != {b:?}");
    }

    #[test]
    fn vertical_axis_reflection_mirrors_x() {
        // Axis at angle pi/2 is a vertical mirror line: reflects x about origin.
        let els = symmetry_elements(SymmetryMode::Axis, false, FRAC_PI_2);
        assert_eq!(els.len(), 1);
        let origin = Point::new(100.0, 100.0);
        let (p, _rot, flip) = els[0].apply(origin, Point::new(130.0, 100.0), 0.0);
        approx(p, Point::new(70.0, 100.0));
        assert!(flip);
    }

    #[test]
    fn horizontal_axis_reflection_mirrors_y() {
        // Axis at angle 0 is a horizontal mirror line: reflects y about origin.
        let els = symmetry_elements(SymmetryMode::Axis, false, 0.0);
        let origin = Point::new(100.0, 100.0);
        let (p, _r, _f) = els[0].apply(origin, Point::new(100.0, 140.0), 0.0);
        approx(p, Point::new(100.0, 60.0));
    }

    #[test]
    fn quadrant_makes_three_copies() {
        assert_eq!(symmetry_elements(SymmetryMode::Quadrant, false, 0.0).len(), 3);
        assert_eq!(symmetry_elements(SymmetryMode::Quadrant, true, 0.0).len(), 3);
    }

    #[test]
    fn radial_makes_seven_copies() {
        assert_eq!(symmetry_elements(SymmetryMode::Radial, false, 0.0).len(), 7);
        assert_eq!(symmetry_elements(SymmetryMode::Radial, true, 0.0).len(), 7);
    }

    #[test]
    fn rotational_preserves_handedness() {
        let els = symmetry_elements(SymmetryMode::Axis, true, FRAC_PI_2);
        let (_p, _r, flip) = els[0].apply(Point::ZERO, Point::new(10.0, 5.0), 0.3);
        assert!(!flip);
    }

    #[test]
    fn centered_config_reproduces_when_assisted() {
        let cfg = GuideConfig::centered(200, 100);
        approx(cfg.origin, Point::new(100.0, 50.0));
        assert!(cfg.reproduces_strokes());
        let sym = Symmetry::from_config(&cfg).expect("assisted symmetry");
        assert_eq!(sym.elements.len(), 1); // single axis mirror = one copy
    }

    #[test]
    fn no_symmetry_when_assist_off_or_non_symmetry_kind() {
        let mut cfg = GuideConfig::centered(10, 10);
        cfg.assisted = false;
        assert!(Symmetry::from_config(&cfg).is_none());
        cfg.assisted = true;
        cfg.kind = GuideKind::Grid2D;
        assert!(Symmetry::from_config(&cfg).is_none());
    }

    #[test]
    fn reset_position_recenters() {
        let mut cfg = GuideConfig::centered(100, 100);
        cfg.origin = Point::new(3.0, 7.0);
        cfg.reset_position(80, 40);
        approx(cfg.origin, Point::new(40.0, 20.0));
    }

    #[test]
    fn grid_assist_snaps_to_nearest_axis() {
        let mut cfg = GuideConfig::centered(200, 200);
        cfg.kind = GuideKind::Grid2D;
        let start = Point::new(50.0, 50.0);
        // A mostly-horizontal drag locks to the horizontal axis, flattening y.
        let lock = assist_lock(&cfg, start, Point::new(150.0, 60.0)).expect("lock");
        let snapped = lock.project(Point::new(150.0, 60.0));
        approx(snapped, Point::new(150.0, 50.0));
    }

    #[test]
    fn perspective_assist_points_at_vanishing_point() {
        let mut cfg = GuideConfig::centered(200, 200);
        cfg.kind = GuideKind::Perspective;
        cfg.vanishing_points = vec![VanishingPoint::new(200.0, 0.0, 0.13)];
        let start = Point::new(0.0, 100.0);
        // Drag roughly toward the VP; the snapped point stays on the start->VP ray.
        let lock = assist_lock(&cfg, start, Point::new(90.0, 60.0)).expect("lock");
        let snapped = lock.project(Point::new(90.0, 60.0));
        // Ray start(0,100)->vp(200,0) has slope -0.5, so at x it is y=100-0.5x.
        assert!((snapped.y - (100.0 - 0.5 * snapped.x)).abs() < 1e-2, "{snapped:?}");
    }

    #[test]
    fn symmetry_never_snaps() {
        let cfg = GuideConfig::centered(100, 100);
        assert!(cfg.reproduces_strokes());
        assert!(!cfg.snaps_strokes());
        assert!(assist_lock(&cfg, Point::ZERO, Point::new(10.0, 3.0)).is_none());
    }

    #[test]
    fn guide_ramp_endpoints_are_black_and_white() {
        assert_eq!(guide_line_color(0.0), (0.0, 0.0, 0.0));
        assert_eq!(guide_line_color(1.0), (1.0, 1.0, 1.0));
        // Out-of-range clamps instead of panicking on the index.
        assert_eq!(guide_line_color(-5.0), (0.0, 0.0, 0.0));
        assert_eq!(guide_line_color(9.9), (1.0, 1.0, 1.0));
    }

    #[test]
    fn pos_from_rgb_snaps_pure_blue_near_blue_stop() {
        // Pure blue is the second ramp stop at t = 1/9.
        let t = guide_pos_from_rgb(0.0, 0.0, 1.0);
        approx_f(t, 1.0 / 9.0);
    }

    #[test]
    fn vp_default_colors_are_distinct_across_points() {
        // A saturated red primary; each point steps hue +60deg, so the three
        // land on visibly different ramp positions.
        let primary = (1.0, 0.0, 0.0);
        let c0 = vp_default_color(0, primary);
        let c1 = vp_default_color(1, primary);
        let c2 = vp_default_color(2, primary);
        assert!((c0 - c1).abs() > 0.05, "{c0} vs {c1}");
        assert!((c1 - c2).abs() > 0.05, "{c1} vs {c2}");
        assert!((c0 - c2).abs() > 0.05, "{c0} vs {c2}");
    }

    fn approx_f(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-2, "{a} != {b}");
    }

    #[test]
    fn config_round_trips_through_json() {
        let mut cfg = GuideConfig::centered(64, 48);
        cfg.symmetry = SymmetryMode::Radial;
        cfg.rotational = true;
        cfg.angle = 0.5;
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: GuideConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cfg, back);
    }
}
