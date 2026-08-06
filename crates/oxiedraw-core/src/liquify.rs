//! Liquify tool: brush-driven local warping of one raster layer.
//!
//! A tool session holds a canvas-space displacement field `D` on the GPU over a
//! pristine snapshot of the layer, and the warped result is always
//! `source(p + D(p))`. Dabs only ever modify `D`.
//!
//! Each *stroke* (pointer down to up) bakes the accumulated field into the layer
//! and records one undo entry, so Ctrl+Z steps back one warp at a time. The
//! snapshot stays pristine across strokes, so N strokes are still a single
//! resample of the original pixels rather than N stacked resamples - the layer
//! is rewritten each time, but never re-read as the source.
//!
//! Pixels outside an active selection are protected: the selection *is* the
//! mask, so the marching ants show what liquify can reach.
//!
//! This module owns the CPU half: the modes, the per-dab parameters handed to
//! the shader, and the symmetry expansion. Because a displacement is a *vector*,
//! a mirrored copy needs the guide element's linear part
//! ([`crate::guides::SymElement::linear`]) applied to the displacement as well
//! as its centre - see [`expand`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxiedraw_utils::geometry::Point;

use crate::enum_meta::EnumMeta;
use crate::guides::Symmetry;

/// Largest number of dabs one GPU pass evaluates. Batches longer than this are
/// split into several passes; see `liquify_ops`.
pub const MAX_DABS_PER_PASS: usize = 32;

/// Default brush diameter in canvas pixels.
pub const DEFAULT_SIZE: f32 = 300.0;
/// Largest brush diameter the tool offers, in canvas pixels. Well past a whole
/// canvas on smaller documents, which is deliberate - a single full-width push
/// is a normal liquify move.
pub const MAX_SIZE: f32 = 5000.0;
/// Default push strength (Photoshop's "Pressure").
pub const DEFAULT_STRENGTH: f32 = 0.5;
/// Default falloff shape (Photoshop's "Density").
pub const DEFAULT_DENSITY: f32 = 0.5;
/// Default hold-in-place accumulation rate.
pub const DEFAULT_RATE: f32 = 0.5;

/// What a liquify dab does to the displacement field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiquifyMode {
    /// Push pixels along the drag direction.
    #[default]
    ForwardWarp,
    /// Rotate pixels around the dab centre.
    Twirl,
    /// Contract pixels toward the dab centre.
    Pucker,
    /// Expand pixels away from the dab centre.
    Bloat,
    /// Push pixels perpendicular to the drag (left of the direction of travel).
    PushLeft,
    /// Ease the field back toward zero, undoing warping locally.
    Reconstruct,
}

impl EnumMeta for LiquifyMode {
    const ALL: &'static [Self] = &[
        Self::ForwardWarp,
        Self::Twirl,
        Self::Pucker,
        Self::Bloat,
        Self::PushLeft,
        Self::Reconstruct,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::ForwardWarp => "Warp",
            Self::Twirl => "Twirl",
            Self::Pucker => "Pucker",
            Self::Bloat => "Bloat",
            Self::PushLeft => "Push Left",
            Self::Reconstruct => "Reconstruct",
        }
    }
}

impl LiquifyMode {
    /// Mode code the shaders branch on. Must match `liquify_compose.frag`.
    #[must_use]
    pub const fn shader_code(self) -> u32 {
        match self {
            Self::ForwardWarp => 0,
            Self::Twirl => 1,
            Self::Pucker => 2,
            Self::Bloat => 3,
            Self::PushLeft => 4,
            Self::Reconstruct => 5,
        }
    }

    /// The mode Alt switches to. Photoshop reverses each tool's sense while the
    /// key is held; modes with no natural opposite stay put.
    #[must_use]
    pub const fn inverted(self) -> Self {
        match self {
            Self::Pucker => Self::Bloat,
            Self::Bloat => Self::Pucker,
            // Twirl / PushLeft reverse via a negative strength, not a different
            // mode, so they invert in place (see [`Self::inverts_by_sign`]).
            other => other,
        }
    }

    /// True when Alt reverses the effect by negating the strength rather than
    /// by switching to another mode (twirl direction, push side).
    #[must_use]
    pub const fn inverts_by_sign(self) -> bool {
        matches!(self, Self::Twirl | Self::PushLeft)
    }

    /// True for modes whose effect only exists while the pointer moves. The
    /// others keep applying when the pointer is held still (Photoshop's "Rate").
    #[must_use]
    pub const fn needs_motion(self) -> bool {
        matches!(self, Self::ForwardWarp | Self::PushLeft)
    }

    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::ForwardWarp => "oxiedraw-liquify-warp-symbolic",
            Self::Twirl => "oxiedraw-liquify-twirl-symbolic",
            Self::Pucker => "oxiedraw-liquify-pucker-symbolic",
            Self::Bloat => "oxiedraw-liquify-bloat-symbolic",
            Self::PushLeft => "oxiedraw-liquify-push-symbolic",
            // Reconstruct rubs warping back out, so it borrows the eraser icon
            // rather than carrying a near-duplicate of it.
            Self::Reconstruct => "oxiedraw-eraser-symbolic",
        }
    }
}

/// One dab as the user painted it, before symmetry expansion. Canvas pixels.
#[derive(Debug, Clone, Copy)]
pub struct LiquifyStamp {
    pub center: Point,
    /// Movement since the previous dab. Drives `ForwardWarp` / `PushLeft`.
    pub drag: Point,
    pub radius: f32,
    /// Falloff shape, `0.0..=1.0`; higher is a harder edge.
    pub density: f32,
    /// Effect strength, `0.0..=1.0`. Negative reverses sign-invertible modes.
    pub strength: f32,
    pub mode: LiquifyMode,
}

/// One dab as the GPU sees it. Layout must match the `Dab` struct in
/// `liquify_compose.frag`: three `vec4`s, std430.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LiquifyDab {
    /// `[center.x, center.y, drag.x, drag.y]`, canvas pixels. `drag` is in the
    /// *un-mirrored* frame; the shader maps it through `linear`.
    pub center_drag: [f32; 4],
    /// Row-major 2x2 linear part of this copy's symmetry element (identity for
    /// the dab the user actually painted).
    pub linear: [f32; 4],
    /// `[radius, density, strength, mode_code]`.
    pub params: [f32; 4],
}

impl LiquifyDab {
    /// The dab's canvas-space AABB, ignoring displacement (the shader's
    /// influence is bounded by `radius`).
    #[must_use]
    pub fn bounds(&self) -> (f32, f32, f32, f32) {
        let (cx, cy, r) = (self.center_drag[0], self.center_drag[1], self.params[0]);
        (cx - r, cy - r, cx + r, cy + r)
    }
}

/// Identity 2x2, for the dab the user actually painted.
const IDENTITY: [f32; 4] = [1.0, 0.0, 0.0, 1.0];

fn to_dab(stamp: &LiquifyStamp, center: Point, linear: [f32; 4]) -> LiquifyDab {
    LiquifyDab {
        center_drag: [center.x, center.y, stamp.drag.x, stamp.drag.y],
        linear,
        params: [
            stamp.radius.max(0.5),
            stamp.density.clamp(0.0, 1.0),
            stamp.strength.clamp(-1.0, 1.0),
            #[allow(clippy::cast_precision_loss)]
            {
                stamp.mode.shader_code() as f32
            },
        ],
    }
}

/// Expand painted dabs into the full set the GPU applies, including every
/// symmetry copy.
///
/// Output is grouped by symmetry element - all the identity dabs first, then all
/// copies of element 0, and so on - rather than interleaved per dab. Each group
/// is spatially tight, which keeps the scissor rect of a GPU pass small; an
/// interleaved order would put a dab and its mirror image (possibly a whole
/// canvas apart) in the same pass and blow the rect up.
#[must_use]
pub fn expand(stamps: &[LiquifyStamp], symmetry: Option<&Symmetry>) -> Vec<LiquifyDab> {
    let copies = symmetry.map_or(0, |s| s.elements.len());
    let mut out = Vec::with_capacity(stamps.len() * (copies + 1));
    for stamp in stamps {
        out.push(to_dab(stamp, stamp.center, IDENTITY));
    }
    let Some(sym) = symmetry else {
        return out;
    };
    for element in &sym.elements {
        let linear = element.linear();
        for stamp in stamps {
            let (center, _rot, _flip) = element.apply(sym.origin, stamp.center, 0.0);
            out.push(to_dab(stamp, center, linear));
        }
    }
    out
}

/// Live state for the Liquify tool, shared between the options bar, the canvas
/// gesture handler, and the session. Same `Rc<Cell>` shape as the other tool
/// states so GTK callbacks can mutate without a message round-trip.
pub struct LiquifyState {
    pub mode: Rc<Cell<LiquifyMode>>,
    /// Brush diameter in canvas pixels.
    pub size: Rc<Cell<f32>>,
    pub strength: Rc<Cell<f32>>,
    pub density: Rc<Cell<f32>>,
    /// How fast the sign-invariant modes accumulate while the pointer is held
    /// still. Zero means they only act on movement.
    pub rate: Rc<Cell<f32>>,
    /// Whether stylus pressure scales `strength`.
    pub pressure_drives_strength: Rc<Cell<bool>>,
    changed: Rc<RefCell<Vec<Box<dyn Fn()>>>>,
}

impl std::fmt::Debug for LiquifyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiquifyState")
            .field("mode", &self.mode.get())
            .field("size", &self.size.get())
            .finish_non_exhaustive()
    }
}

impl Clone for LiquifyState {
    fn clone(&self) -> Self {
        Self {
            mode: Rc::clone(&self.mode),
            size: Rc::clone(&self.size),
            strength: Rc::clone(&self.strength),
            density: Rc::clone(&self.density),
            rate: Rc::clone(&self.rate),
            pressure_drives_strength: Rc::clone(&self.pressure_drives_strength),
            changed: Rc::clone(&self.changed),
        }
    }
}

impl LiquifyState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            mode: Rc::new(Cell::new(LiquifyMode::default())),
            size: Rc::new(Cell::new(DEFAULT_SIZE)),
            strength: Rc::new(Cell::new(DEFAULT_STRENGTH)),
            density: Rc::new(Cell::new(DEFAULT_DENSITY)),
            rate: Rc::new(Cell::new(DEFAULT_RATE)),
            pressure_drives_strength: Rc::new(Cell::new(true)),
            changed: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// The mode a dab should use given whether Alt is held, plus the strength
    /// sign that goes with it.
    #[must_use]
    pub fn resolve_mode(&self, alt: bool) -> (LiquifyMode, f32) {
        let mode = self.mode.get();
        let strength = self.strength.get().clamp(0.0, 1.0);
        if !alt {
            return (mode, strength);
        }
        if mode.inverts_by_sign() {
            (mode, -strength)
        } else {
            (mode.inverted(), strength)
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
}

impl Default for LiquifyState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
// Exact comparison is the point in these tests: the dab payload is copied
// verbatim into the GPU buffer, so an exact `[1, 0, 0, 1]` is what identity
// means. Approximate checks use the `approx` helper below.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::guides::{GuideConfig, SymElement, SymmetryMode, symmetry_elements};
    use std::f32::consts::FRAC_PI_2;

    fn stamp(center: Point, drag: Point, mode: LiquifyMode) -> LiquifyStamp {
        LiquifyStamp {
            center,
            drag,
            radius: 40.0,
            density: 0.5,
            strength: 0.5,
            mode,
        }
    }

    /// Apply a row-major 2x2 to a vector.
    fn mul(m: [f32; 4], v: (f32, f32)) -> (f32, f32) {
        (m[0] * v.0 + m[1] * v.1, m[2] * v.0 + m[3] * v.1)
    }

    fn approx(a: (f32, f32), b: (f32, f32)) {
        assert!(
            (a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4,
            "{a:?} != {b:?}",
        );
    }

    #[test]
    fn reflect_linear_is_an_involution_with_negative_determinant() {
        let m = SymElement::Reflect { axis: FRAC_PI_2 }.linear();
        let det = m[0] * m[3] - m[1] * m[2];
        assert!((det + 1.0).abs() < 1e-5, "reflection determinant {det}");
        // Reflecting twice is the identity.
        let v = (3.0, -7.0);
        approx(mul(m, mul(m, v)), v);
    }

    #[test]
    fn rotate_linear_preserves_length_and_determinant() {
        let m = SymElement::Rotate { angle: 0.7 }.linear();
        let det = m[0] * m[3] - m[1] * m[2];
        assert!((det - 1.0).abs() < 1e-5, "rotation determinant {det}");
        let v = (3.0, -7.0);
        let r = mul(m, v);
        let len = |p: (f32, f32)| p.0.hypot(p.1);
        assert!((len(r) - len(v)).abs() < 1e-4);
    }

    /// A vertical mirror line must turn a rightward push into a leftward one -
    /// the property that makes mirrored liquify strokes meet at the axis.
    #[test]
    fn vertical_mirror_reverses_a_horizontal_push() {
        let m = SymElement::Reflect { axis: FRAC_PI_2 }.linear();
        approx(mul(m, (1.0, 0.0)), (-1.0, 0.0));
        // Vertical motion is untouched by a vertical mirror line.
        approx(mul(m, (0.0, 1.0)), (0.0, 1.0));
    }

    /// A reflection flips handedness, so the tangential (twirl) field it
    /// produces spins the other way. `M * perp(M^T u) == -perp(u)`.
    #[test]
    fn reflection_reverses_twirl_handedness() {
        let m = SymElement::Reflect { axis: 0.3 }.linear();
        let transpose = [m[0], m[2], m[1], m[3]];
        let u = (5.0, 2.0);
        let local = mul(transpose, u);
        let perp_local = (-local.1, local.0);
        approx(mul(m, perp_local), (u.1, -u.0));
    }

    /// A pure rotation keeps handedness, so a twirl copy spins the same way.
    #[test]
    fn rotation_preserves_twirl_handedness() {
        let m = SymElement::Rotate { angle: 1.1 }.linear();
        let transpose = [m[0], m[2], m[1], m[3]];
        let u = (5.0, 2.0);
        let local = mul(transpose, u);
        let perp_local = (-local.1, local.0);
        approx(mul(m, perp_local), (-u.1, u.0));
    }

    /// Pucker / bloat are radial, so mirroring must not change them:
    /// `M * normalize(M^T u) == normalize(u)`.
    #[test]
    fn radial_modes_are_mirror_invariant() {
        for m in [
            SymElement::Reflect { axis: 0.9 }.linear(),
            SymElement::Rotate { angle: 2.2 }.linear(),
        ] {
            let transpose = [m[0], m[2], m[1], m[3]];
            let u = (4.0, -3.0);
            let local = mul(transpose, u);
            let len = local.0.hypot(local.1);
            let unit_local = (local.0 / len, local.1 / len);
            approx(mul(m, unit_local), (u.0 / 5.0, u.1 / 5.0));
        }
    }

    #[test]
    fn expand_without_symmetry_is_identity() {
        let s = [stamp(Point::new(10.0, 20.0), Point::new(1.0, 0.0), LiquifyMode::ForwardWarp)];
        let dabs = expand(&s, None);
        assert_eq!(dabs.len(), 1);
        assert_eq!(dabs[0].linear, IDENTITY);
        assert_eq!(dabs[0].center_drag, [10.0, 20.0, 1.0, 0.0]);
    }

    #[test]
    fn expand_mirrors_center_and_carries_the_linear_part() {
        let cfg = GuideConfig::centered(200, 200);
        let sym = Symmetry::from_config(&cfg).expect("assisted axis symmetry");
        let s = [stamp(Point::new(130.0, 100.0), Point::new(1.0, 0.0), LiquifyMode::ForwardWarp)];
        let dabs = expand(&s, Some(&sym));
        assert_eq!(dabs.len(), 2);
        // Mirrored about x = 100.
        assert!((dabs[1].center_drag[0] - 70.0).abs() < 1e-3);
        // The drag stays un-mirrored in the buffer; the matrix does the work.
        assert_eq!(dabs[1].center_drag[2], 1.0);
        approx(mul(dabs[1].linear, (1.0, 0.0)), (-1.0, 0.0));
    }

    /// Dabs are emitted grouped by symmetry element, so one GPU pass covers a
    /// spatially tight region instead of a dab plus its far-away mirror.
    #[test]
    fn expand_groups_by_symmetry_element() {
        let elements = symmetry_elements(SymmetryMode::Quadrant, false, 0.0);
        let sym = Symmetry { origin: Point::new(50.0, 50.0), elements };
        let stamps: Vec<_> = (0..4)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let x = 10.0 + i as f32;
                stamp(Point::new(x, 10.0), Point::ZERO, LiquifyMode::Bloat)
            })
            .collect();
        let dabs = expand(&stamps, Some(&sym));
        assert_eq!(dabs.len(), 4 * 4);
        // The first group is the un-mirrored stamps, in order.
        for (i, dab) in dabs.iter().take(4).enumerate() {
            assert_eq!(dab.linear, IDENTITY);
            #[allow(clippy::cast_precision_loss)]
            let expected = 10.0 + i as f32;
            assert!((dab.center_drag[0] - expected).abs() < 1e-4);
        }
        // Each subsequent group shares one matrix.
        for group in 1..4 {
            let m = dabs[group * 4].linear;
            for dab in &dabs[group * 4..group * 4 + 4] {
                assert_eq!(dab.linear, m);
            }
        }
    }

    #[test]
    fn alt_inverts_pucker_and_bloat_by_mode() {
        let state = LiquifyState::new();
        state.mode.set(LiquifyMode::Pucker);
        let (mode, strength) = state.resolve_mode(true);
        assert_eq!(mode, LiquifyMode::Bloat);
        assert!(strength > 0.0);
    }

    #[test]
    fn alt_inverts_twirl_by_sign() {
        let state = LiquifyState::new();
        state.mode.set(LiquifyMode::Twirl);
        let (mode, strength) = state.resolve_mode(true);
        assert_eq!(mode, LiquifyMode::Twirl);
        assert!(strength < 0.0);
    }

    #[test]
    fn dab_bounds_cover_the_radius() {
        let s = [stamp(Point::new(100.0, 50.0), Point::ZERO, LiquifyMode::Twirl)];
        let (x0, y0, x1, y1) = expand(&s, None)[0].bounds();
        assert!((x0 - 60.0).abs() < 1e-4 && (x1 - 140.0).abs() < 1e-4);
        assert!((y0 - 10.0).abs() < 1e-4 && (y1 - 90.0).abs() < 1e-4);
    }

    #[test]
    fn mode_codes_are_distinct() {
        let mut codes: Vec<u32> = LiquifyMode::ALL.iter().map(|m| m.shader_code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), LiquifyMode::ALL.len());
    }
}
