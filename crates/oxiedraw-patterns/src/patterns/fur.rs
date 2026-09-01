//! Fur, drawn as one continuous edge rather than a row of separate tufts.
//!
//! A rank is built as one walk from end to end (see [`build_silhouette`]) that
//! sweeps along the body, rises over a tuft, comes back down in an S and carries
//! on, and only then gets filled or inked. That one boundary is why the tips
//! join up and nothing pierces anything else; overlaying separate tufts can
//! approximate the silhouette but never the line. Shaping on top of it is
//! hierarchical - low-frequency noise decides where the fur heads and how tall
//! it stands, so tufts come in runs and cowlicks.

use crate::geom::{GeometrySink, RibbonPoint, contains};
use crate::noise::{fbm_1d, smoothstep, smoothstep_range};
use crate::params::{ParamDef, ParamKind};
use crate::rng::Rng;
use crate::scatter::{self, Root, ScatterSpec};
use crate::{GenCtx, Pattern};

// Random channels, drawn per root so a tuft's length is independent of its
// angle and of every other tuft's.
const CH_LENGTH: u8 = 0;
const CH_ANGLE: u8 = 1;
const CH_MEMBERS: u8 = 5;
const CH_BACK: u8 = 11;

// Angle limits, in radians, at the knob's full value.
const MAX_FLOW: f32 = 1.15;
const MAX_ANGLE_JITTER: f32 = 0.55;
const MAX_DIRECTION_SWING: f32 = 1.10;

const SWIRL_SALT: u64 = 0x5717_0F1E;
const BODY_SALT: u64 = 0xB0D1_1EFE;
/// Wavelength of the direction swirl, in tuft spacings.
const SWIRL_SCALE: f32 = 5.0;
/// Keeps each depth rank's scatter ids in its own range.
const RANK_ID_STRIDE: u32 = 1_000_000;

/// Resample step before occlusion, fine enough that a thin shape cannot slip
/// between two samples and let the line run through it.
const CLIP_STEP: f32 = 2.5;
/// Rate the pen's weight breathes along a run, in radians per pixel.
const PEN_WAVE: f32 = 0.035;
/// Step of the return leg that closes the boundary, in ribbon pixels.
const RETURN_STEP: f32 = 6.0;
/// How far a rank behind keeps clear of the line in front, in pen widths. Any
/// closer and the two read as one line doubled rather than as depth.
const INK_CLEARANCE: f32 = 2.6;
/// Shortest surviving ink run, in half-pen-widths.
const MIN_RUN: f32 = 14.0;
/// How far the fur fades back into the line at each end, in tuft spacings.
const END_FADE: f32 = 0.55;

/// Sweep bow, as a fraction of the point it climbs to.
const CONCAVE: f32 = 0.55;
/// Where the sweep's first control sits and how far under the direct path it
/// pulls. Late and low, so the hand leaves the last point travelling along the
/// drawn line; pulled out over the path instead it reads as a sawtooth.
const CONCAVE_BIAS: f32 = 0.72;
const CONCAVE_DIP: f32 = 0.42;
/// The second control, which together with `CONCAVE_*` is the direction the
/// hand arrives in: straight in gives a needle, flattening off leaves the tuft
/// lying over with a gap under it for the pull-back to hook into.
const ARRIVE: f32 = 0.22;
const NEEDLE_APPROACH: f32 = 0.55;
const CURL_APPROACH: f32 = 0.05;
/// Saturates earlier than the other bend-driven behaviour: a tuft either lies
/// over or it does not.
const CURL_GAIN: f32 = 2.6;
const CURL_HOOK: f32 = 1.9;
/// How much of the room under a sweep a pull-back may use. Under one, so the
/// two hug rather than touch - that is a needle tip.
const HUG: f32 = 0.82;
/// Shortest pull-back worth inking. The pen tapers in and out over a run, so
/// anything shorter comes out at no width at all.
const MIN_BACK: f32 = 1.5;
const PULL_BACK_BOW: f32 = 0.35;
const PULL_BACK_STEPS: usize = 6;
const SWEEP_STEPS: usize = 10;
/// How far a pull-back may eat into the sweep before it. Past this the hand
/// ends up behind where it started and the edge stops progressing.
const MAX_PULL_BACK: f32 = 0.72;
const MIN_SWEEP: f32 = 3.0;

/// A clump's spread within the tuft's stretch, and how much shorter and
/// shallower the pull-backs inside it are than the one that ends it.
const CLUMP_REACH: f32 = 0.55;
const CLUMP_PULL: f32 = 0.35;
const CLUMP_NOTCH: f32 = 0.30;
/// The gap below which two points count as one tuft, as a fraction of spacing.
const CLUMP_GAP: f32 = 0.55;

/// The least a pull-back falls for the distance it travels back, so a hook
/// always has an opening between it and the sweep it runs under.
const CLAW_SLOPE: f32 = 0.5;
/// How far a pull-back falls, and how far down it may go relative to the body.
/// The notch between two tufts is most of what says fur rather than wobbly line.
const REVERSE_DROP: f32 = 0.75;
const REVERSE_DEEP: f32 = 1.0;
const REVERSE_FLOOR: f32 = 0.6;

/// Each point carries its own stretch of curve, rather than only the tallest of
/// a handful surviving as it did when tufts were drawn overlapping.
const TIP_GAIN: f32 = 1.55;
/// Bend radius that counts as fully bent, in pixels.
const CURVE_REFERENCE: f32 = 200.0;

// What the body's shape does to the fur. Flat fur lies down in long sweeps, a
// bulge fans it open and stands it up, a hollow crowds it flatter still.
const STRAIGHT_HEIGHT: f32 = 0.30;
const STRAIGHT_PUCK: f32 = 1.30;
/// High keeps a puck rare: most of a flat run stays down, one tuft stands.
const PUCK_RARITY: f32 = 2.6;
const BENT_HEIGHT: f32 = 2.15;
/// Fewer tufts survive on the flat, so the ones that do get long stretches to
/// themselves and the base spacing can stay tight enough for a bend to cluster.
const STRAIGHT_DENSITY: f32 = 0.42;
/// A tuft covers more line when flat; on a bend it takes all the room it has.
const STRAIGHT_SPAN: f32 = 1.85;
const BENT_SPAN: f32 = 1.0;
const SQUEEZED_HEIGHT: f32 = 0.62;
const THIN_HEIGHT: f32 = 0.30;

/// How much of the curve a tuft occupies per unit of its width.
const SPAN_SCALE: f32 = 2.2;
/// How far a lean, and then a bend on top of it, throw the peak off the centre
/// of a tuft's stretch.
const LEAN_SKEW: f32 = 0.42;
const BEND_SKEW: f32 = 0.18;

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

pub struct Fur;

pub const SCHEMA: &[ParamDef] = &[
    ParamDef {
        id: "curve_a",
        label: "Curv. A",
        group: "Shape",
        tooltip: "How the sweep into each point bows. Negative hollows it out so the hand runs along the line and turns up at the end - a needle; positive flattens the arrival so the tuft lies over and hooks back",
        kind: ParamKind::Float {
            min: -1.0,
            max: 1.0,
            default: 0.0,
            step: 0.01,
        },
        modulatable: true,
        exposed: true,
    },
    ParamDef {
        id: "curve_b",
        label: "Curv. B",
        group: "Shape",
        tooltip: "How the pull-back off each point bows. It can only ever bow away from the sweep above it - toward would mean crossing it, which reads as a scribble",
        kind: ParamKind::Float {
            min: -1.0,
            max: 1.0,
            default: 0.0,
            step: 0.01,
        },
        modulatable: true,
        exposed: true,
    },
    ParamDef {
        id: "flip_side",
        label: "Flip side",
        group: "Shape",
        tooltip: "Grow the fur on the other side of the line",
        kind: ParamKind::Bool { default: true },
        modulatable: false,
        exposed: true,
    },
    ParamDef {
        id: "length",
        label: "Height",
        group: "Shape",
        tooltip: "How far the fur stands up off the line",
        kind: ParamKind::Float {
            min: 2.0,
            max: 400.0,
            default: 38.0,
            step: 1.0,
        },
        modulatable: true,
        exposed: true,
    },
    ParamDef {
        id: "offset",
        label: "Offset",
        group: "Shape",
        tooltip: "Slide the whole rank across the line, in heights. Negative sinks it into the body so only the tips show; positive lifts it clear of the line",
        kind: ParamKind::Float {
            min: -1.0,
            max: 1.0,
            // Sunk a full height, so the drawn line is where the tips reach and
            // the fur reads as growing from inside the body rather than off it.
            default: -1.0,
            step: 0.01,
        },
        modulatable: false,
        exposed: true,
    },
    ParamDef {
        id: "thickness",
        label: "Thick",
        group: "Shape",
        tooltip: "How much of the line one tuft takes up. Against Height this is what decides whether the fur lies along the line or stands up off it",
        kind: ParamKind::Float {
            min: 0.5,
            max: 160.0,
            default: 44.0,
            step: 0.5,
        },
        modulatable: true,
        exposed: true,
    },
    ParamDef {
        id: "spacing",
        label: "Spacing",
        group: "Shape",
        tooltip: "Gap between tufts along the line",
        kind: ParamKind::Float {
            min: 2.0,
            max: 320.0,
            default: 62.0,
            step: 1.0,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "density",
        label: "Density",
        group: "Shape",
        tooltip: "Fraction of tufts kept - bind this to pressure for fur that thickens where you press",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 1.0,
            step: 0.01,
        },
        modulatable: true,
        exposed: false,
    },
    ParamDef {
        id: "flow",
        label: "Flow",
        group: "Shape",
        tooltip: "How far the fur sweeps toward the drawing direction",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 0.78,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "taper",
        label: "Taper",
        group: "Shape",
        tooltip: "How hollow each forward sweep is. High runs the hand along the body and turns it up sharply into the point; low climbs more directly",
        kind: ParamKind::Float {
            min: 0.0,
            max: 2.0,
            default: 1.0,
            step: 0.05,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "direction",
        label: "Direction swing",
        group: "Variation",
        tooltip: "How far the fur wanders off the flow direction, and how strongly it curls with or against it. Neighbouring tufts swing together, so the fur forms cowlicks rather than pointing at random",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 0.45,
            step: 0.01,
        },
        modulatable: true,
        exposed: false,
    },
    ParamDef {
        id: "length_variation",
        label: "Length variation",
        group: "Variation",
        tooltip: "How much lengths differ from tuft to tuft",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 0.62,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "clump_scale",
        label: "Wave scale",
        group: "Variation",
        tooltip: "Length of the long/short waves, in tuft spacings",
        kind: ParamKind::Float {
            min: 1.0,
            max: 20.0,
            default: 2.2,
            step: 0.5,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "regularity",
        label: "Regularity",
        group: "Variation",
        tooltip: "1.0 places tufts on exact centres, which reads as machine-made",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 0.30,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "angle_jitter",
        label: "Angle jitter",
        group: "Variation",
        tooltip: "Random lean added per spike",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 0.35,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "clump_size",
        label: "Spikes per tuft",
        group: "Clumping",
        tooltip: "Upper bound on the spikes sharing one root",
        kind: ParamKind::Int {
            min: 1,
            max: 6,
            default: 2,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "clump_spread",
        label: "Clump spread",
        group: "Clumping",
        tooltip: "How wide the outer spikes of a tuft fan out",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 0.35,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "clump_falloff",
        label: "Clump falloff",
        group: "Clumping",
        tooltip: "How much shorter the outer spikes of a tuft are",
        kind: ParamKind::Float {
            min: 0.0,
            max: 0.9,
            default: 0.50,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "layers",
        label: "Layers",
        group: "Depth",
        tooltip: "Depth ranks; back ranks are shorter and denser",
        kind: ParamKind::Int {
            min: 1,
            max: 4,
            default: 2,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "layer_depth",
        label: "Layer falloff",
        group: "Depth",
        tooltip: "How much shorter the back ranks are",
        kind: ParamKind::Float {
            min: 0.0,
            max: 0.9,
            default: 0.35,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "back_alpha",
        label: "Back opacity",
        group: "Depth",
        tooltip: "Coverage of the furthest rank. Below 1.0 the ranks read as depth, but the mass is no longer one flat colour",
        kind: ParamKind::Float {
            min: 0.05,
            max: 1.0,
            default: 1.0,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "ink_detail",
        label: "Ink detail",
        group: "Line art",
        tooltip: "Line art only: how much of the fur under the top layer gets drawn. Low keeps the drawing open; high fills it with interior marks",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 0.0,
            step: 0.01,
        },
        modulatable: true,
        exposed: false,
    },
    ParamDef {
        id: "base_width",
        label: "Base",
        group: "Body",
        tooltip: "Solid mass along the line, on top of the body the fur already has",
        kind: ParamKind::Float {
            min: 0.0,
            max: 60.0,
            default: 0.0,
            step: 0.5,
        },
        modulatable: true,
        exposed: false,
    },
    ParamDef {
        id: "body",
        label: "Body",
        group: "Body",
        tooltip: "How far off the line the edge lies between its tufts, as a fraction of the fur's length. Zero drops the edge to the line between every tuft, which reads as a row of teeth; a little gives the brushed edge fur actually has",
        kind: ParamKind::Float {
            min: 0.0,
            max: 0.8,
            default: 0.22,
            step: 0.01,
        },
        modulatable: true,
        exposed: false,
    },
    ParamDef {
        id: "curve_response",
        label: "Follow the bend",
        group: "Body",
        tooltip: "How much the fur answers the shape of the body under it: longer and fanned into points over a bulge, shorter and flatter in a hollow",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 0.55,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "reverse",
        label: "Pull back",
        group: "Stroke",
        tooltip: "How far the hand pulls back after each point before sweeping forward again, as a share of the tuft. This is the short reverse stroke that gives fur its notches",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 0.34,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
    ParamDef {
        id: "clutter",
        label: "Clutter",
        group: "Stroke",
        tooltip: "How much the pull-backs differ from one another. Low draws every notch much the same; high has some barely lift off the point and others come most of the way home",
        kind: ParamKind::Float {
            min: 0.0,
            max: 1.0,
            default: 0.55,
            step: 0.01,
        },
        modulatable: false,
        exposed: false,
    },
];

impl Pattern for Fur {
    fn id(&self) -> &'static str {
        "fur"
    }

    fn label(&self) -> &'static str {
        "Fur"
    }

    fn description(&self) -> &'static str {
        "Layered tufts sweeping along the line"
    }

    fn schema(&self) -> &'static [ParamDef] {
        SCHEMA
    }

    fn generate(&self, ctx: &GenCtx<'_>, out: &mut GeometrySink) {
        // Off the unmodulated height, so the shift stays constant rather than
        // wobbling with whatever is bound to length.
        out.set_bias(ctx.params.float("length") * ctx.params.float("offset"));
        let layers = ctx.params.count("layers").clamp(1, 8);
        let mut ranks: Vec<Silhouette> = Vec::new();
        for rank in 0..layers {
            // 0 at the back rank, 1 at the front. A single layer is the front.
            let depth_t = if layers > 1 {
                rank as f32 / (layers - 1) as f32
            } else {
                1.0
            };
            if let Some(built) = build_silhouette(ctx, rank, depth_t) {
                ranks.push(built);
            }
        }

        match ctx.style.ink_width() {
            None => {
                for rank in &ranks {
                    out.fill(rank.area.clone(), rank.alpha, rank.depth);
                }
            }
            Some(width) => {
                let detail = ctx.params.float("ink_detail");
                ink_silhouettes(out, &ranks, width, detail);
            }
        }
    }
}

/// One rank's fur, as a single continuous boundary.
struct Silhouette {
    /// The outer edge, start of the curve to end.
    path: Vec<RibbonPoint>,
    /// The same edge closed back along the curve, for occlusion tests.
    area: Vec<RibbonPoint>,
    /// The same edge as the movements that drew it.
    strokes: Vec<Stroke>,
    alpha: f32,
    depth: i16,
}

/// Draw one rank's edge as a run of hand movements: points along the curve,
/// with a sweep into each one and a pull-back off it.
fn build_silhouette(ctx: &GenCtx<'_>, rank: u32, depth_t: f32) -> Option<Silhouette> {
    let params = &ctx.params;
    let rest = ctx.field.rest_length();
    if rest <= 0.0 {
        return None;
    }
    let spacing = params.float("spacing") * lerp(0.72, 1.0, depth_t);
    let length_scale = lerp(1.0 - params.float("layer_depth"), 1.0, depth_t);
    let response = params.float("curve_response");
    let rng = ctx.rng.salted(u64::from(rank) + 1);

    let roots = scatter::along(
        ctx.field,
        &ScatterSpec {
            spacing,
            regularity: params.float("regularity"),
            id_offset: rank.saturating_mul(RANK_ID_STRIDE),
        },
        rng,
        |frame| {
            // Thin the fur over a flat run so the survivors get long stretches
            // of curve to themselves, reading as sweeps rather than a fringe.
            let spike = (frame.bulge * CURVE_REFERENCE * response).clamp(0.0, 1.0);
            ctx.value("density", frame).clamp(0.0, 1.0) * lerp(STRAIGHT_DENSITY, 1.0, spike)
        },
    );

    let mut tips: Vec<Tip> = roots
        .iter()
        .flat_map(|root| plan_tip(ctx, rng, root, spacing, length_scale))
        .collect();
    tips.sort_by(|a, b| a.s.total_cmp(&b.s));

    let strokes = chain(ctx, &tips, rest, spacing, rng);
    // Filling wants one path, and so does testing what a rank in front covers.
    let path: Vec<RibbonPoint> = strokes
        .iter()
        .flat_map(|stroke| stroke.points.iter().copied())
        .collect();
    if path.len() < 2 {
        return None;
    }

    Some(Silhouette {
        area: closed(&path),
        path,
        strokes,
        alpha: lerp(params.float("back_alpha"), 1.0, depth_t),
        depth: i16::try_from(rank).unwrap_or(i16::MAX),
    })
}

/// How far off the curve the fur lies between its tufts. Never zero: a boundary
/// that returned to the line between every tuft reads as a row of teeth.
fn base_level(ctx: &GenCtx<'_>, s: f32, spacing: f32, rng: Rng) -> f32 {
    let frame = ctx.field.sample(s);
    // Explicit mass is there whether or not fur grows out of it; the body the
    // fur makes by lying down is fur, so that part thins with density.
    let mass = ctx.value("base_width", &frame);
    let density = ctx.value("density", &frame).clamp(0.0, 1.0);
    let body = ctx.value("length", &frame) * ctx.params.float("body");
    let wave = fbm_1d(rng.salted(BODY_SALT).seed(), s / (spacing * 3.0).max(1.0), 2);
    mass + body * (0.55 + 0.9 * wave) * lerp(THIN_HEIGHT, 1.0, density)
}

/// One tip of fur, and how the hand gets to it and away from it.
struct Tip {
    /// Where the point sits, in ribbon space.
    s: f32,
    n: f32,
    /// How far the sweep up to it swings off the direct path: hollow at zero,
    /// arching over the point at one.
    bow: f32,
    curl: f32,
    /// How far the pull-back travels against the sweep, and how far it falls.
    back: f32,
    drop: f32,
}

fn plan_tip(
    ctx: &GenCtx<'_>,
    rng: Rng,
    root: &Root,
    spacing: f32,
    length_scale: f32,
) -> Vec<Tip> {
    let params = &ctx.params;
    let frame = &root.frame;

    // Raised to a power so most tufts stay low and a few stand well up.
    let wave_span = (spacing * params.float("clump_scale")).max(1.0);
    let wave = fbm_1d(rng.seed(), root.s / wave_span, 2).powf(2.0);
    let variation = params.float("length_variation");
    let macro_factor = lerp(1.0 - variation, 1.0 + variation * 0.9, wave);
    let micro_factor = 1.0 + variation * 0.25 * rng.signed(root.id, CH_LENGTH);

    // Shaped rather than linear, so a barely-there curve stays flat and a real
    // bulge commits: the turn should read as an event, not a gradient.
    let bend = (frame.bulge * CURVE_REFERENCE * params.float("curve_response")).clamp(-1.0, 1.0);
    let spike = smoothstep(bend.max(0.0));
    let squeeze = smoothstep((-bend).max(0.0));

    // Density has to reach the height too: dropping tufts alone barely lowers
    // the edge once they overlap, since the ones left still reach as far.
    let density = ctx.value("density", frame).clamp(0.0, 1.0);

    // A puck on the flat has to be earned, so most of a flat run stays down.
    let puck = wave.powf(PUCK_RARITY);
    let height = (ctx.value("length", frame)
        * length_scale
        * TIP_GAIN
        * macro_factor
        * micro_factor
        * lerp(lerp(STRAIGHT_HEIGHT, STRAIGHT_PUCK, puck), BENT_HEIGHT, spike)
        * lerp(1.0, SQUEEZED_HEIGHT, squeeze)
        * lerp(THIN_HEIGHT, 1.0, density))
        .max(1.0);

    let span = (ctx.value("thickness", frame)
        * SPAN_SCALE
        * lerp(STRAIGHT_SPAN, BENT_SPAN, spike))
    .max(2.0);

    // Neighbouring tufts read almost the same value from these low-frequency
    // fields, so the fur forms cowlicks rather than pointing at random.
    let swirl_span = (spacing * SWIRL_SCALE).max(1.0);
    let direction = ctx.value("direction", frame);
    let swirl = fbm_1d(rng.salted(SWIRL_SALT).seed(), root.s / swirl_span, 2).mul_add(2.0, -1.0);
    let jitter = params.float("angle_jitter") * MAX_ANGLE_JITTER;
    let lean = ((params.float("flow") * MAX_FLOW
        + direction * MAX_DIRECTION_SWING * swirl
        + jitter * rng.signed(root.id, CH_ANGLE))
        / MAX_FLOW)
        .clamp(-1.6, 1.6);

    // The point sits late in the tuft's stretch when the fur is sweeping
    // forward: that is what a lean is.
    let skew = (lean * LEAN_SKEW + spike * BEND_SKEW * lean.signum()).clamp(-0.47, 0.47);
    let valley = base_level(ctx, root.s, spacing, rng);

    // Clutter decides how much the pull-back varies: low keeps them much the
    // same, high has some barely lift off the point and others come home.
    let clutter = params.float("clutter").clamp(0.0, 1.0);
    let roll = rng.unit(root.id, CH_BACK);
    let spread = lerp(1.0, roll * 2.0, clutter);
    // Curl and pull-back travel together - a curl with a short pull-back is a
    // blunt tip - and `curve_a` rides the same axis the bend drives.
    let curl = (spike * CURL_GAIN + params.float("curve_a")).clamp(0.0, 1.0);
    let back = span * params.float("reverse") * spread * lerp(1.0, CURL_HOOK, curl);
    // Of the point's own height, not its height above the line: a thick body
    // under the fur otherwise cancels the notch out entirely.
    let drop = height * lerp(REVERSE_DROP, REVERSE_DEEP, roll);

    // A clump is several points close together, with short pull-backs between
    // them where separate tufts get long ones.
    let members = rng.count(root.id, CH_MEMBERS, 1, params.count("clump_size").clamp(1, 8));
    let spread = params.float("clump_spread").clamp(0.05, 1.0);
    let falloff = params.float("clump_falloff");
    let hollow = params.float("taper");
    (0..members)
            .map(|member| {
                let even = if members > 1 {
                    (member as f32 + 0.5) / members as f32 - 0.5
                } else {
                    0.0
                };
                let scale = if members > 1 {
                    1.0 - falloff * (member as f32 / (members - 1) as f32)
                } else {
                    1.0
                };
                let member_height = height * scale;
                Tip {
                    s: root.s + span * (skew + even * spread * CLUMP_REACH),
                    n: valley + member_height,
                    bow: (member_height * CONCAVE * hollow).max(0.5),
                    curl,
                    back: back.max(0.0),
                    drop: drop.max(0.0) * scale,
                }
        })
        .collect()
}

/// One movement of the hand: a sweep forward, or the short pull back after it.
struct Stroke {
    points: Vec<RibbonPoint>,
    reverse: bool,
}

/// Run the tips together into one edge: sweep forward into a point, pull back
/// off it - near for a tight cluster, far for a loose one - and sweep again.
fn chain(
    ctx: &GenCtx<'_>,
    tips: &[Tip],
    rest: f32,
    spacing: f32,
    rng: Rng,
) -> Vec<Stroke> {
    let mut strokes: Vec<Stroke> = Vec::new();
    let mut pen = RibbonPoint::new(0.0, base_level(ctx, 0.0, spacing, rng), 0.0);
    let fade = (spacing * END_FADE).max(1.0);

    for (index, tip) in tips.iter().enumerate() {
        // Fur fades back into the line at the ends of a stroke, so a point that
        // falls in the fade is drawn shorter rather than cut off mid-sweep.
        let ends = smoothstep_range(0.0, fade, tip.s) * smoothstep_range(0.0, fade, rest - tip.s);
        let body = base_level(ctx, tip.s, spacing, rng);
        let peak = body + (tip.n - body) * ends;
        // Only points already passed are skipped. A point below the pen is
        // still drawn; skipping those dropped most of a flat run.
        if tip.s <= pen.s + MIN_SWEEP {
            continue;
        }

        let point = RibbonPoint::new(tip.s, peak, 0.0);
        let forward = sweep(pen, point, tip.bow, tip.curl);

        // The pull-back is the undercut: off the point, back and down the near
        // side of the tuft just drawn. Held level instead it crosses the sweep.
        let room = (tip.s - pen.s) * MAX_PULL_BACK;
        // Decided by the next point along the curve, not by which tuft planned
        // it: two tufts' points interleave freely, and points close together
        // should give one tuft with a nick rather than two.
        let gap = tips.get(index + 1).map_or(rest, |next| next.s) - tip.s;
        let near = 1.0 - smoothstep_range(0.0, (spacing * CLUMP_GAP).max(1.0), gap);
        let want_back = tip.back * ends * lerp(1.0, CLUMP_PULL, near);
        // A minimum fall for the distance travelled back. Without it a long
        // hook comes back as a hairline parallel to the sweep, with no notch.
        let fall = (tip.drop * ends * lerp(1.0, CLUMP_NOTCH, near))
            .max(want_back * CLAW_SLOPE)
            .min((peak - body * REVERSE_FLOOR).max(0.0));
        // How far back it can come and still stay under the sweep. A steep
        // arrival leaves almost no room (a needle); an arched one leaves a long
        // gap for the undercut to run back into (the hook of a curl).
        let reach = clearance(&forward, point, fall) * HUG;
        let back = want_back.min(room).min(reach);

        // The pull-back is shaped against the sweep, so it is built before the
        // sweep is handed over.
        let back_stroke = (back.hypot(fall) >= MIN_BACK).then(|| {
            let landing = RibbonPoint::new(tip.s - back, peak - fall, 0.0);
            (
                pull_back(point, landing, ctx.params.float("curve_b"), &forward),
                landing,
            )
        });
        strokes.push(Stroke {
            points: forward,
            reverse: false,
        });
        if let Some((points, landing)) = back_stroke {
            strokes.push(Stroke {
                points,
                reverse: true,
            });
            pen = landing;
        } else {
            pen = point;
        }
    }

    // Home along the body.
    if pen.s < rest {
        let tail = RibbonPoint::new(rest, base_level(ctx, rest, spacing, rng), 0.0);
        strokes.push(Stroke {
            points: sweep(pen, tail, 0.0, 0.0),
            reverse: false,
        });
    }
    strokes
}

/// How far a pull-back can travel back from `tip` and still stay under the
/// sweep that came up to it, given how far it falls on the way. Every sample of
/// the sweep caps it and the tightest cap wins; reading the arrival slope alone
/// would licence a pull-back straight through an arched sweep.
fn clearance(sweep: &[RibbonPoint], tip: RibbonPoint, fall: f32) -> f32 {
    sweep
        .iter()
        .filter(|p| p.s < tip.s && p.n < tip.n)
        .map(|p| fall * (tip.s - p.s) / (tip.n - p.n))
        .fold(f32::MAX, f32::min)
}

/// The pull-back off a point: back and down, bowed by `bow`, and held under the
/// sweep it came up. A straight pull-back stays under a rising sweep by
/// construction; a bowed one does not, so the clamp is what lets `bow` be free.
fn pull_back(
    tip: RibbonPoint,
    landing: RibbonPoint,
    bow: f32,
    sweep: &[RibbonPoint],
) -> Vec<RibbonPoint> {
    let (ds, dn) = (landing.s - tip.s, landing.n - tip.n);
    let span = ds.hypot(dn);
    if bow.abs() < 1e-3 || span <= f32::EPSILON {
        return vec![tip, landing];
    }
    // Perpendicular to the chord, so the bow is the same size whichever way
    // the pull-back leans.
    let lift = bow * span * PULL_BACK_BOW;
    let control = RibbonPoint::new(
        (tip.s + landing.s).mul_add(0.5, -dn / span * lift),
        (tip.n + landing.n).mul_add(0.5, ds / span * lift),
        0.0,
    );
    (0..=PULL_BACK_STEPS)
        .map(|i| {
            let t = i as f32 / PULL_BACK_STEPS as f32;
            let u = 1.0 - t;
            let s = u * u * tip.s + 2.0 * u * t * control.s + t * t * landing.s;
            let n = u * u * tip.n + 2.0 * u * t * control.n + t * t * landing.n;
            let ceiling = height_on(sweep, s).unwrap_or(f32::MAX);
            RibbonPoint::new(s, n.min(ceiling), 0.0)
        })
        .collect()
}

/// How high a forward-going polyline runs at `s`, or `None` past its ends.
fn height_on(path: &[RibbonPoint], s: f32) -> Option<f32> {
    path.windows(2).find_map(|pair| {
        let (a, b) = (pair[0], pair[1]);
        if s < a.s || s > b.s {
            return None;
        }
        let span = b.s - a.s;
        Some(if span.abs() < f32::EPSILON {
            a.n.max(b.n)
        } else {
            a.n + (b.n - a.n) * (s - a.s) / span
        })
    })
}

/// The forward sweep between two points. Two controls because the halves answer
/// to different things - `bow` holds the way out under the direct path, `curl`
/// shapes the arrival - and one control moving both reads as a sawtooth.
fn sweep(from: RibbonPoint, to: RibbonPoint, bow: f32, curl: f32) -> Vec<RibbonPoint> {
    let curl = curl.clamp(0.0, 1.0);
    let run = to.s - from.s;
    let low = (from.n.min(to.n) - bow * CONCAVE_DIP).max(0.0);
    let out = RibbonPoint::new(from.s + run * CONCAVE_BIAS, low, 0.0);
    // The arrival direction: still climbing for a needle, nearly level with the
    // point for a curl.
    let approach = lerp(NEEDLE_APPROACH, CURL_APPROACH, curl);
    let land = RibbonPoint::new(to.s - run * ARRIVE, to.n - (to.n - low) * approach, 0.0);
    (0..=SWEEP_STEPS)
        .map(|i| {
            let t = i as f32 / SWEEP_STEPS as f32;
            let u = 1.0 - t;
            // Cubic through the two controls.
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            RibbonPoint::new(
                a * from.s + b * out.s + c * land.s + d * to.s,
                a * from.n + b * out.n + c * land.n + d * to.n,
                0.0,
            )
        })
        .collect()
}

/// The boundary closed back along the curve, so it can be filled or tested
/// against. The return leg is stepped because ribbon space is straight and the
/// curve is not: one long segment maps to a chord and fills everything under it.
fn closed(path: &[RibbonPoint]) -> Vec<RibbonPoint> {
    let mut area = path.to_vec();
    let (Some(first), Some(last)) = (path.first().copied(), path.last().copied()) else {
        return area;
    };
    let span = last.s - first.s;
    let steps = ((span.abs() / RETURN_STEP).ceil() as usize).clamp(1, 4096);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        area.push(RibbonPoint::new(last.s - span * t, 0.0, 0.0));
    }
    area
}

/// Ink every rank, back to front, hiding whatever the ranks in front cover.
/// `detail` decides how much of what lies behind the front rank is drawn.
fn ink_silhouettes(out: &mut GeometrySink, ranks: &[Silhouette], width: f32, detail: f32) {
    let last = ranks.len().saturating_sub(1);
    // Fixed step, so distances between ranks can be measured point to point.
    let lines: Vec<Vec<RibbonPoint>> = ranks
        .iter()
        .map(|rank| resample(&rank.path, CLIP_STEP))
        .collect();
    let clearance = width * INK_CLEARANCE;

    for (index, rank) in ranks.iter().enumerate() {
        // The front rank is drawn in full, the ones behind it more lightly.
        let weight = if index == last {
            1.0
        } else {
            detail.clamp(0.0, 1.0) * 0.8
        };
        if weight <= 0.0 {
            continue;
        }
        let half = width * 0.5 * weight;
        let areas: Vec<&[RibbonPoint]> =
            ranks[index + 1..].iter().map(|r| r.area.as_slice()).collect();
        let fronts = &lines[index + 1..];
        // Covered by a rank in front, or too close alongside its line. Without
        // the second test a hidden rank surfaces where the front one dips and
        // lays a second line beside the first, reading as a stray mark.
        let hidden = |p: &RibbonPoint| {
            areas.iter().any(|area| contains(area, p.s, p.n))
                || fronts.iter().any(|line| within(line, p, clearance))
        };
        // One stroke per movement, keeping its direction. Adjacent strokes
        // share an endpoint so they union into one continuous line.
        for stroke in &rank.strokes {
            // Dense enough that a thin shape cannot slip between two samples.
            let dense = resample(&stroke.points, CLIP_STEP);
            for run in visible_runs(&dense, &hidden, half) {
                out.stroke(run, rank.alpha, rank.depth, stroke.reverse);
            }
        }
    }
}

/// Both are sampled at [`CLIP_STEP`], so comparing points is close enough and
/// much cheaper than projecting onto every segment.
fn within(line: &[RibbonPoint], p: &RibbonPoint, clearance: f32) -> bool {
    let limit = clearance * clearance;
    line.iter().any(|q| {
        let (ds, dn) = (q.s - p.s, q.n - p.n);
        ds.mul_add(ds, dn * dn) <= limit
    })
}

fn resample(path: &[RibbonPoint], step: f32) -> Vec<RibbonPoint> {
    let mut out = Vec::with_capacity(path.len() * 2);
    for pair in path.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let span = (b.s - a.s).hypot(b.n - a.n);
        let steps = ((span / step).ceil() as usize).max(1);
        for i in 0..steps {
            let t = i as f32 / steps as f32;
            out.push(RibbonPoint::new(
                a.s + (b.s - a.s) * t,
                a.n + (b.n - a.n) * t,
                0.0,
            ));
        }
    }
    if let Some(last) = path.last() {
        out.push(*last);
    }
    // The pen tapers in at one end and out at the other, so a run with nothing
    // between its ends is inked at zero width. Short pull-backs land here.
    if out.len() == 2 {
        let mid = RibbonPoint::new(
            (out[0].s + out[1].s) * 0.5,
            (out[0].n + out[1].n) * 0.5,
            0.0,
        );
        out.insert(1, mid);
    }
    out
}

/// The stretches `hidden` does not reject, each given the pen's width profile.
fn visible_runs(
    path: &[RibbonPoint],
    hidden: &impl Fn(&RibbonPoint) -> bool,
    half: f32,
) -> Vec<Vec<RibbonPoint>> {
    if path.len() < 2 {
        return Vec::new();
    }
    // A fragment shorter than this is a fleck rather than a mark. A movement
    // that was never clipped is kept whatever its length, since the pull-backs
    // are short by design and dropping them punches holes in the line.
    let shortest = (half * MIN_RUN).max(3.0);
    let untouched = !path.iter().any(&hidden);
    let shortest = if untouched { 0.0 } else { shortest };

    let mut runs: Vec<Vec<RibbonPoint>> = Vec::new();
    let mut current: Vec<RibbonPoint> = Vec::new();
    let mut was_hidden = hidden(&path[0]);
    if !was_hidden {
        current.push(path[0]);
    }
    for pair in path.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let now_hidden = hidden(&b);
        if now_hidden == was_hidden {
            if !now_hidden {
                current.push(b);
            }
            continue;
        }
        let cut = crossing(a, b, was_hidden, hidden);
        if was_hidden {
            current.clear();
            current.push(cut);
            current.push(b);
        } else {
            current.push(cut);
            push_run(&mut runs, std::mem::take(&mut current), shortest);
        }
        was_hidden = now_hidden;
    }
    push_run(&mut runs, current, shortest);
    for run in &mut runs {
        pen(run, half);
    }
    runs
}

/// Tapered at both ends where the pen lifts, breathing in between, so the line
/// has weight rather than being a wire.
fn pen(run: &mut [RibbonPoint], half: f32) {
    let last = run.len().saturating_sub(1);
    if last == 0 {
        return;
    }
    let total: f32 = run
        .windows(2)
        .map(|p| (p[1].s - p[0].s).hypot(p[1].n - p[0].n))
        .sum();
    // Only a short taper: runs meet end to end, so a long one would thin the
    // line to nothing at every join and leave a dotted edge.
    let ramp = (total * 0.2).min(half * 1.2).max(f32::EPSILON);
    let mut travelled = 0.0;
    for i in 0..=last {
        if i > 0 {
            travelled += (run[i].s - run[i - 1].s).hypot(run[i].n - run[i - 1].n);
        }
        let ends = smoothstep_range(0.0, ramp, travelled)
            * smoothstep_range(0.0, ramp, (total - travelled).max(0.0));
        let breath = 0.72 + 0.28 * (travelled * PEN_WAVE).sin();
        run[i].half_width = half * ends * breath;
    }
}

fn push_run(runs: &mut Vec<Vec<RibbonPoint>>, run: Vec<RibbonPoint>, shortest: f32) {
    if run.len() < 2 {
        return;
    }
    let length: f32 = run
        .windows(2)
        .map(|p| (p[1].s - p[0].s).hypot(p[1].n - p[0].n))
        .sum();
    if length >= shortest {
        runs.push(run);
    }
}

/// Where visibility flips between `a` and `b`, found by bisection.
fn crossing(
    a: RibbonPoint,
    b: RibbonPoint,
    a_hidden: bool,
    hidden: &impl Fn(&RibbonPoint) -> bool,
) -> RibbonPoint {
    let mix = |t: f32| {
        RibbonPoint::new(a.s + (b.s - a.s) * t, a.n + (b.n - a.n) * t, a.half_width)
    };
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    for _ in 0..6 {
        let mid = 0.5 * (lo + hi);
        if hidden(&mix(mid)) == a_hidden {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    mix(0.5 * (lo + hi))
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::params::{ModSource, Modulation, Params};
    use crate::spine::{Side, SpineNode};
    use crate::{PatternRequest, generate};
    use oxiedraw_utils::geometry::Point;

    /// The schema defaults with `offset` neutralised. It slides the whole rank
    /// across the line and its default moves, so leaving it in would have these
    /// shape tests measuring where the rank was parked.
    fn shape_defaults() -> Params {
        let mut params = Fur.defaults();
        params.set_float("offset", 0.0);
        params
    }

    fn nodes(len: f32, start_pressure: f32, end_pressure: f32) -> Vec<SpineNode> {
        vec![
            SpineNode::new(Point::new(0.0, 0.0), start_pressure),
            SpineNode::new(Point::new(len, 0.0), end_pressure),
        ]
    }

    fn request<'a>(
        nodes: &'a [SpineNode],
        params: &'a Params,
        bindings: &'a crate::params::Bindings,
        seed: u64,
    ) -> PatternRequest<'a> {
        PatternRequest {
            pattern: &Fur,
            params,
            bindings,
            nodes,
            seed,
            side: Side::Left,
            style: crate::StrokeStyle::Fill,
            rest_length: None,
        }
    }

    /// Fold the geometry into a value that changes if anything about it does.
    fn fingerprint(geometry: &crate::PatternGeometry) -> u64 {
        let mut hash = 0xCBF2_9CE4_8422_2325_u64;
        for element in geometry.to_canvas() {
            for v in &element.verts {
                for value in [v.pos.x, v.pos.y, v.half_width] {
                    hash ^= u64::from(value.to_bits());
                    hash = hash.wrapping_mul(0x100_0000_01B3);
                }
            }
        }
        hash
    }

    #[test]
    fn the_schema_is_well_formed() {
        let mut ids: Vec<&str> = SCHEMA.iter().map(|d| d.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate parameter id in the fur schema");
        for def in SCHEMA {
            assert!(!def.label.is_empty(), "{} has no label", def.id);
            assert!(!def.group.is_empty(), "{} has no group", def.id);
            if let ParamKind::Float { min, max, default, .. } = def.kind {
                assert!(min < max, "{} has an empty range", def.id);
                assert!(
                    (min..=max).contains(&default),
                    "{} defaults outside its range",
                    def.id
                );
            }
        }
    }

    // Any element below full coverage shows up as shading inside the mass.
    #[test]
    fn the_default_look_is_a_single_flat_silhouette() {
        let n = nodes(400.0, 1.0, 1.0);
        let params = shape_defaults();
        let bindings = crate::params::Bindings::default();
        let geometry = generate(&request(&n, &params, &bindings, 8)).expect("geometry");
        for element in geometry.to_canvas() {
            assert!(
                (element.alpha - 1.0).abs() < 1e-6,
                "element at depth {} is only {} opaque",
                element.depth,
                element.alpha
            );
        }
    }

    #[test]
    fn defaults_produce_fur() {
        let n = nodes(300.0, 1.0, 1.0);
        let params = shape_defaults();
        let bindings = crate::params::Bindings::default();
        let geometry = generate(&request(&n, &params, &bindings, 1)).expect("geometry");
        // One boundary per rank: the detail lives in the vertices.
        assert!(geometry.element_count() >= 2);
        assert!(
            geometry.vertex_count() > 80,
            "only {} vertices of boundary",
            geometry.vertex_count()
        );
        assert!(geometry.to_canvas().iter().all(|e| e.fill));
    }

    #[test]
    fn generation_is_deterministic() {
        let n = nodes(300.0, 1.0, 1.0);
        let params = shape_defaults();
        let bindings = crate::params::Bindings::default();
        let a = generate(&request(&n, &params, &bindings, 42)).expect("geometry");
        let b = generate(&request(&n, &params, &bindings, 42)).expect("geometry");
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn a_different_seed_gives_different_fur() {
        let n = nodes(300.0, 1.0, 1.0);
        let params = shape_defaults();
        let bindings = crate::params::Bindings::default();
        let a = generate(&request(&n, &params, &bindings, 1)).expect("geometry");
        let b = generate(&request(&n, &params, &bindings, 2)).expect("geometry");
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fur_grows_on_the_chosen_side() {
        let n = nodes(300.0, 1.0, 1.0);
        let params = shape_defaults();
        let bindings = crate::params::Bindings::default();
        let mut left = request(&n, &params, &bindings, 7);
        left.side = Side::Left;
        let mut right = request(&n, &params, &bindings, 7);
        right.side = Side::Right;

        // The curve runs left to right along y = 0, so its left normal points
        // up the screen. Only the heel may cross to the far side, and never by
        // more than a spike's width.
        let slack = crate::Resolved::new(SCHEMA, &params).float("thickness");
        let above = generate(&left).expect("geometry");
        for element in above.to_canvas() {
            for v in &element.verts {
                assert!(
                    v.pos.y <= slack,
                    "left-side fur dipped below the line: {}",
                    v.pos.y
                );
            }
        }
        let below = generate(&right).expect("geometry");
        for element in below.to_canvas() {
            for v in &element.verts {
                assert!(
                    v.pos.y >= -slack,
                    "right-side fur rose above the line: {}",
                    v.pos.y
                );
            }
        }
    }

    // The edge is meant to travel back after every point. What must hold is
    // that it never goes back further than the sweep it just made, or the
    // drawing stops progressing and piles up on itself.
    #[test]
    fn a_pull_back_never_undoes_the_sweep_before_it() {
        let n = nodes(700.0, 1.0, 1.0);
        let bindings = crate::params::Bindings::default();
        // Crowded and cluttered: every setting that lengthens a pull-back.
        let mut params = shape_defaults();
        params.set_float("thickness", 70.0);
        params.set_float("spacing", 30.0);
        params.set_float("reverse", 1.0);
        params.set_float("clutter", 1.0);
        params.set_float("flow", 1.0);
        params.set_float("direction", 1.0);
        params.set_int("clump_size", 5);
        params.set_int("layers", 1);

        for seed in 0..12 {
            let mut req = request(&n, &params, &bindings, seed);
            req.style = crate::StrokeStyle::Ink { width: 4.0 };
            let canvas = generate(&req).expect("geometry").to_canvas();
            // The curve runs along y = 0 from x = 0, so x tracks position.
            let travel = |e: &crate::CanvasElement| {
                let (first, last) = (e.verts[0].pos.x, e.verts[e.verts.len() - 1].pos.x);
                last - first
            };
            let mut forward = 0.0_f32;
            for element in &canvas {
                if element.verts.len() < 2 {
                    continue;
                }
                if element.reverse {
                    let back = -travel(element);
                    assert!(
                        back <= forward + 0.5,
                        "seed {seed}: pulled back {back} after a sweep of {forward}"
                    );
                } else {
                    forward = travel(element);
                    assert!(
                        forward >= -0.5,
                        "seed {seed}: a forward sweep ran backwards by {forward}"
                    );
                }
            }
        }
    }

    // A curl flattens the arrival so the tuft lies over. What must not come
    // with it is the sweep leaving the drawn line early, which reads as a
    // sawtooth - and is what one control carrying both halves gets you.
    #[test]
    fn a_curled_sweep_arrives_flatter_but_still_lies_along_the_line() {
        let from = RibbonPoint::new(0.0, 0.0, 0.0);
        let to = RibbonPoint::new(100.0, 50.0, 0.0);
        let needle = sweep(from, to, 20.0, 0.0);
        let curled = sweep(from, to, 20.0, 1.0);

        let arrival = |path: &[RibbonPoint]| {
            let last = path.len() - 1;
            let (a, b) = (path[last - 1], path[last]);
            (b.n - a.n) / (b.s - a.s)
        };
        assert!(
            arrival(&curled) < arrival(&needle) * 0.5,
            "curl did not flatten the arrival: {} against {}",
            arrival(&curled),
            arrival(&needle)
        );
        for (name, path) in [("needle", &needle), ("curled", &curled)] {
            let crossed = path.iter().find(|p| p.s >= 50.0).expect("reaches halfway");
            assert!(
                crossed.n < 25.0,
                "{name} sweep stood {} off the line at the halfway point",
                crossed.n
            );
        }
    }

    // The rule that makes a tip read as a point rather than a scribble: the
    // pull-back may hug the sweep as closely as it likes but never cut over it.
    // Crossing once means the next sweep starts above and crosses back.
    #[test]
    fn a_pull_back_never_crosses_the_sweep_that_drew_it() {
        crossing_check(0.0, 0.0);
    }

    // A bow toward the sweep is precisely what the rule forbids, so both
    // extremes of `curve_b` have to stay under it.
    #[test]
    fn bowing_the_pull_back_cannot_break_the_rule() {
        crossing_check(0.0, 1.0);
        crossing_check(0.0, -1.0);
        crossing_check(1.0, 1.0);
        crossing_check(-1.0, -1.0);
    }

    fn crossing_check(curve_a: f32, curve_b: f32) {
        let n = nodes(700.0, 1.0, 1.0);
        let bindings = crate::params::Bindings::default();
        // One rank, so nothing is clipped and each movement is one element.
        let mut params = shape_defaults();
        params.set_int("layers", 1);
        params.set_float("reverse", 1.0);
        params.set_float("clutter", 1.0);
        params.set_int("clump_size", 4);
        params.set_float("curve_a", curve_a);
        params.set_float("curve_b", curve_b);

        for seed in 0..12 {
            let mut req = request(&n, &params, &bindings, seed);
            req.style = crate::StrokeStyle::Ink { width: 4.0 };
            let canvas = generate(&req).expect("geometry").to_canvas();
            // The guide runs along y = 0 from x = 0, so x is s and -y is n.
            let mut sweep: Vec<(f32, f32)> = Vec::new();
            let mut checked = 0;
            for element in &canvas {
                let points: Vec<(f32, f32)> =
                    element.verts.iter().map(|v| (v.pos.x, -v.pos.y)).collect();
                if !element.reverse {
                    sweep = points;
                    continue;
                }
                let (Some(&(tip_s, tip_n)), Some(&(land_s, land_n))) =
                    (points.first(), points.last())
                else {
                    continue;
                };
                for step in 0_u8..=20 {
                    let t = f32::from(step) / 20.0;
                    let s = land_s + (tip_s - land_s) * t;
                    let n = land_n + (tip_n - land_n) * t;
                    let Some(under) = height_at(&sweep, s) else {
                        continue;
                    };
                    assert!(
                        n <= under + 0.5,
                        "seed {seed}: pull-back stood {} above its sweep at s = {s}",
                        n - under
                    );
                    checked += 1;
                }
            }
            assert!(checked > 0, "seed {seed}: no pull-back was measured at all");
        }
    }

    /// How high a forward-going polyline runs at `s`, or `None` past its ends.
    fn height_at(path: &[(f32, f32)], s: f32) -> Option<f32> {
        path.windows(2).find_map(|pair| {
            let ((a_s, a_n), (b_s, b_n)) = (pair[0], pair[1]);
            if s < a_s || s > b_s {
                return None;
            }
            let span = b_s - a_s;
            Some(if span.abs() < f32::EPSILON {
                a_n.max(b_n)
            } else {
                a_n + (b_n - a_n) * (s - a_s) / span
            })
        })
    }

    #[test]
    fn both_sides_generates_twice_as_much() {
        let n = nodes(300.0, 1.0, 1.0);
        let params = shape_defaults();
        let bindings = crate::params::Bindings::default();
        let one = generate(&request(&n, &params, &bindings, 3)).expect("geometry");
        let mut both_req = request(&n, &params, &bindings, 3);
        both_req.side = Side::Both;
        let both = generate(&both_req).expect("geometry");
        assert!(both.element_count() > one.element_count());
        let has_above = both
            .to_canvas()
            .iter()
            .any(|e| e.verts.iter().any(|v| v.pos.y < -5.0));
        let has_below = both
            .to_canvas()
            .iter()
            .any(|e| e.verts.iter().any(|v| v.pos.y > 5.0));
        assert!(has_above && has_below, "Both did not grow on both sides");
    }

    #[test]
    fn spacing_controls_how_many_tufts_there_are() {
        let n = nodes(400.0, 1.0, 1.0);
        let bindings = crate::params::Bindings::default();
        let mut wide = shape_defaults();
        wide.set_float("spacing", 200.0);
        let mut tight = shape_defaults();
        tight.set_float("spacing", 55.0);
        let sparse = generate(&request(&n, &wide, &bindings, 5)).expect("geometry");
        let dense = generate(&request(&n, &tight, &bindings, 5)).expect("geometry");
        // Tufts overlap and share peaks, so how much fur is there is what
        // changes, not the peak count.
        assert!(
            mean_reach(&dense, f32::MIN, f32::MAX) > mean_reach(&sparse, f32::MIN, f32::MAX) * 1.3,
            "{} of fur vs {}",
            mean_reach(&dense, f32::MIN, f32::MAX),
            mean_reach(&sparse, f32::MIN, f32::MAX)
        );
    }

    /// How far the edge stands off the line on average. Tufts overlap and
    /// merge, so this says how much fur is there where counting peaks cannot.
    fn mean_reach(geometry: &crate::PatternGeometry, from: f32, to: f32) -> f32 {
        let canvas = geometry.to_canvas();
        let Some(edge) = canvas.first() else {
            return 0.0;
        };
        let picked: Vec<f32> = edge
            .verts
            .iter()
            .filter(|v| v.pos.x >= from && v.pos.x < to)
            .map(|v| -v.pos.y)
            .collect();
        if picked.is_empty() {
            return 0.0;
        }
        picked.iter().sum::<f32>() / picked.len() as f32
    }

    /// A tight arc, walked so the default growing side is inside it and its
    /// surface is therefore a bulge - the only place shape-driven behaviour
    /// shows, since a flat run lies down and a hollow is crowded flat.
    fn arc(radius: f32) -> Vec<SpineNode> {
        arc_pressed(radius, 1.0, 1.0)
    }

    fn arc_pressed(radius: f32, start: f32, end: f32) -> Vec<SpineNode> {
        (0..=60)
            .map(|i| {
                let t = i as f32 / 60.0;
                let a = std::f32::consts::PI * t;
                SpineNode::new(
                    Point::new(500.0 + a.cos() * radius, 500.0 - a.sin() * radius),
                    start + (end - start) * t,
                )
            })
            .collect()
    }

    /// Averaged over a stretch given as a fraction of the way along. Measured
    /// from the centre so the curve itself does not skew the reading.
    fn arc_stand(
        geometry: &crate::PatternGeometry,
        centre: Point,
        radius: f32,
        from: f32,
        to: f32,
    ) -> f32 {
        let canvas = geometry.to_canvas();
        let picked: Vec<f32> = canvas
            .iter()
            .flat_map(|e| e.verts.iter())
            .filter_map(|v| {
                let angle = (centre.y - v.pos.y).atan2(v.pos.x - centre.x);
                let t = angle / std::f32::consts::PI;
                (t >= from && t < to).then(|| radius - v.pos.distance(centre))
            })
            .collect();
        if picked.is_empty() {
            return 0.0;
        }
        picked.iter().sum::<f32>() / picked.len() as f32
    }

    /// The fur grows into the bulge, so a point of it is a local minimum of
    /// distance from the centre.
    fn arc_peaks(geometry: &crate::PatternGeometry, centre: Point) -> usize {
        let canvas = geometry.to_canvas();
        let Some(edge) = canvas.first() else {
            return 0;
        };
        edge.verts
            .windows(3)
            .filter(|w| {
                let d: Vec<f32> = w.iter().map(|v| v.pos.distance(centre)).collect();
                d[1] < d[0] && d[1] <= d[2]
            })
            .count()
    }

    // Measured on a bend: on a flat run `clump_size` is deliberately inert.
    #[test]
    fn clumping_adds_peaks_on_a_bend() {
        let n = arc(150.0);
        let centre = Point::new(500.0, 500.0);
        let bindings = crate::params::Bindings::default();
        let mut single = shape_defaults();
        single.set_int("layers", 1);
        single.set_int("clump_size", 1);
        let mut clumped = single.clone();
        clumped.set_int("clump_size", 5);

        let plain = generate(&request(&n, &single, &bindings, 5)).expect("geometry");
        let many = generate(&request(&n, &clumped, &bindings, 5)).expect("geometry");
        assert!(arc_peaks(&plain, centre) > 0, "no peaks at all");
        assert!(
            arc_peaks(&many, centre) > arc_peaks(&plain, centre),
            "clumping added no peaks: {} vs {}",
            arc_peaks(&plain, centre),
            arc_peaks(&many, centre)
        );
    }

    #[test]
    fn fur_lies_down_on_the_flat_and_stands_up_on_a_bend() {
        let bindings = crate::params::Bindings::default();
        let mut params = shape_defaults();
        params.set_int("layers", 1);

        let flat = generate(&request(&nodes(900.0, 1.0, 1.0), &params, &bindings, 7))
            .expect("geometry");
        let flat_reach = flat
            .to_canvas()
            .iter()
            .flat_map(|e| e.verts.iter())
            .map(|v| -v.pos.y)
            .fold(0.0_f32, f32::max);

        let curve = arc(150.0);
        let centre = Point::new(500.0, 500.0);
        let bent = generate(&request(&curve, &params, &bindings, 7)).expect("geometry");
        let bent_reach = bent
            .to_canvas()
            .iter()
            .flat_map(|e| e.verts.iter())
            .map(|v| 150.0 - v.pos.distance(centre))
            .fold(0.0_f32, f32::max);

        assert!(
            bent_reach > flat_reach * 1.8,
            "a bend should stand the fur up: flat {flat_reach}, bent {bent_reach}"
        );
    }

    // Offset moves the rank without touching its shape: sinking it far enough
    // puts the roots on the far side while the fur still grows the same way,
    // which is what makes it not a flip.
    #[test]
    fn offset_slides_the_fur_across_the_line_without_reshaping_it() {
        let bindings = crate::params::Bindings::default();
        let n = nodes(900.0, 1.0, 1.0);
        let mut params = shape_defaults();
        params.set_int("layers", 1);
        let height = crate::params::Resolved::new(SCHEMA, &params).float("length");

        // Reach measured off the line, on the growing side (screen -y).
        let reach = |params: &Params| -> (f32, f32) {
            let canvas = generate(&request(&n, params, &bindings, 17))
                .expect("geometry")
                .to_canvas();
            canvas
                .iter()
                .flat_map(|e| e.verts.iter())
                .map(|v| -v.pos.y)
                .fold((f32::MAX, f32::MIN), |(lo, hi), y| (lo.min(y), hi.max(y)))
        };

        // Explicit, rather than leaning on a default that moves.
        params.set_float("offset", 0.0);
        let (base_lo, base_hi) = reach(&params);
        params.set_float("offset", -0.5);
        let (sunk_lo, sunk_hi) = reach(&params);

        let shift = height * 0.5;
        assert!(
            (base_lo - sunk_lo - shift).abs() < 1.0,
            "the root did not move a half height: {base_lo} -> {sunk_lo}"
        );
        assert!(
            ((base_hi - base_lo) - (sunk_hi - sunk_lo)).abs() < 1.0,
            "the fur changed shape: {} -> {}",
            base_hi - base_lo,
            sunk_hi - sunk_lo
        );
        assert!(
            sunk_lo < 0.0,
            "half a height down should put the roots across the line, at {sunk_lo}"
        );
    }

    #[test]
    fn layers_stack_into_depth_ranks() {
        let n = nodes(200.0, 1.0, 1.0);
        let bindings = crate::params::Bindings::default();
        let mut params = shape_defaults();
        params.set_int("layers", 3);
        let geometry = generate(&request(&n, &params, &bindings, 5)).expect("geometry");
        let mut depths: Vec<i16> = geometry.to_canvas().iter().map(|e| e.depth).collect();
        depths.sort_unstable();
        depths.dedup();
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn back_ranks_are_dimmer_than_the_front() {
        let n = nodes(200.0, 1.0, 1.0);
        let bindings = crate::params::Bindings::default();
        let mut params = shape_defaults();
        params.set_int("layers", 2);
        params.set_float("back_alpha", 0.5);
        let geometry = generate(&request(&n, &params, &bindings, 5)).expect("geometry");
        let canvas = geometry.to_canvas();
        let back = canvas.iter().find(|e| e.depth == 0).expect("back rank");
        let front = canvas.iter().find(|e| e.depth == 1).expect("front rank");
        assert!((back.alpha - 0.5).abs() < 1e-5, "back alpha {}", back.alpha);
        assert!((front.alpha - 1.0).abs() < 1e-5, "front alpha {}", front.alpha);
    }

    #[test]
    fn the_edge_keeps_a_body_and_base_width_thickens_it() {
        let n = nodes(400.0, 1.0, 1.0);
        let bindings = crate::params::Bindings::default();

        let reach = |params: &Params| {
            let geometry = generate(&request(&n, params, &bindings, 5)).expect("geometry");
            // Fur grows to negative y, so the highest the edge ever gets in y
            // is its body level.
            geometry
                .to_canvas()
                .iter()
                .flat_map(|e| e.verts.iter())
                // Ignore the return leg that closes the shape along the line.
                .filter(|v| v.pos.y < -0.01)
                .map(|v| -v.pos.y)
                .fold(f32::INFINITY, f32::min)
        };

        let mut flat = shape_defaults();
        flat.set_float("body", 0.0);
        flat.set_float("base_width", 0.0);
        let mut bodied = shape_defaults();
        bodied.set_float("base_width", 0.0);
        let mut thick = shape_defaults();
        thick.set_float("base_width", 40.0);

        assert!(
            reach(&bodied) > reach(&flat),
            "the default edge should stand off the line between tufts"
        );
        assert!(
            reach(&thick) > reach(&bodied) + 20.0,
            "base width should add solid mass under the fur"
        );
    }

    #[test]
    fn density_bound_to_pressure_thins_the_light_end() {
        let n = nodes(1200.0, 0.05, 1.0);
        let mut params = shape_defaults();
        params.set_int("layers", 1);
        // One peak per root, so this counts roots kept rather than how many
        // points each tuft happened to have.
        params.set_int("clump_size", 1);
        let mut bindings = crate::params::Bindings::default();
        bindings.set("density", Modulation::new(ModSource::Pressure, 1.0));
        let geometry = generate(&request(&n, &params, &bindings, 11)).expect("geometry");
        // Mean pressure is 0.29 over the light half and 0.76 over the firm one.
        // Measured as how much fur is there, since overlapping tufts share a
        // peak and a count would miss them.
        let light = mean_reach(&geometry, 0.0, 600.0);
        let firm = mean_reach(&geometry, 600.0, 1200.0);
        assert!(
            firm > light * 1.5,
            "pressure did not thin the light end: {light} vs {firm}"
        );
    }

    // Measured on a bend and as a mean, not a high-water mark: on a flat run
    // the fur is thinned right out, so whether a tuft lands inside the measured
    // window is luck and the reading is the body's thickness.
    #[test]
    fn length_bound_to_pressure_shortens_the_light_end() {
        let radius = 150.0;
        let centre = Point::new(500.0, 500.0);
        let n = arc_pressed(radius, 0.1, 1.0);
        let mut params = shape_defaults();
        params.set_float("length_variation", 0.0);
        params.set_int("layers", 1);
        params.set_int("clump_size", 1);
        let mut bindings = crate::params::Bindings::default();
        bindings.set("length", Modulation::new(ModSource::Pressure, 1.0));
        let geometry = generate(&request(&n, &params, &bindings, 13)).expect("geometry");
        let light = arc_stand(&geometry, centre, radius, 0.15, 0.35);
        let firm = arc_stand(&geometry, centre, radius, 0.65, 0.85);
        assert!(firm > light * 2.0, "light {light} vs firm {firm}");
    }

    // Movements are inked as separate strokes, so what matters is that each
    // picks up exactly where the last left off.
    #[test]
    fn the_movements_of_the_hand_join_up_into_one_line() {
        let n = nodes(500.0, 1.0, 1.0);
        let bindings = crate::params::Bindings::default();
        let mut params = shape_defaults();
        params.set_int("layers", 1);
        let mut req = request(&n, &params, &bindings, 5);
        req.style = crate::StrokeStyle::Ink { width: 4.0 };
        let canvas = generate(&req).expect("geometry").to_canvas();
        assert!(canvas.len() > 3, "expected a run of movements");
        assert!(canvas.iter().all(|e| !e.fill));
        assert!(
            canvas.iter().any(|e| e.reverse),
            "no pull-backs were drawn at all"
        );

        for pair in canvas.windows(2) {
            let end = pair[0].verts[pair[0].verts.len() - 1].pos;
            let start = pair[1].verts[0].pos;
            assert!(
                end.distance(start) < 1.0,
                "the hand jumped from {end:?} to {start:?}"
            );
        }
    }

    #[test]
    fn ink_behind_never_lands_alongside_the_line_in_front() {
        let n = arc(150.0);
        let bindings = crate::params::Bindings::default();
        let mut params = shape_defaults();
        params.set_int("layers", 2);

        let pen = 4.0_f32;
        let mut req = request(&n, &params, &bindings, 23);
        req.style = crate::StrokeStyle::Ink { width: pen };
        let canvas = generate(&req).expect("geometry").to_canvas();

        let front: Vec<_> = canvas.iter().filter(|e| e.depth == 1).collect();
        let behind: Vec<_> = canvas.iter().filter(|e| e.depth == 0).collect();
        assert!(!front.is_empty(), "no front rank was drawn");

        // Allow for the front line being sampled every CLIP_STEP: a point can
        // sit half a step nearer the true line than to the nearest sample.
        let limit = pen * INK_CLEARANCE * 0.8;
        for back in &behind {
            for v in &back.verts {
                let nearest = front
                    .iter()
                    .flat_map(|e| e.verts.iter())
                    .map(|f| f.pos.distance(v.pos))
                    .fold(f32::INFINITY, f32::min);
                assert!(
                    nearest >= limit,
                    "a hidden rank drew {nearest} px from the front line"
                );
            }
        }
    }

    #[test]
    fn a_degenerate_curve_produces_nothing() {
        let params = shape_defaults();
        let bindings = crate::params::Bindings::default();
        let single = vec![SpineNode::new(Point::new(5.0, 5.0), 1.0)];
        assert!(generate(&request(&single, &params, &bindings, 1)).is_none());
        assert!(generate(&request(&[], &params, &bindings, 1)).is_none());
    }

    #[test]
    fn remapping_an_edited_curve_keeps_every_element() {
        let n = nodes(300.0, 1.0, 1.0);
        let params = shape_defaults();
        let bindings = crate::params::Bindings::default();
        let mut geometry = generate(&request(&n, &params, &bindings, 21)).expect("geometry");
        let before = geometry.element_count();
        let before_shape = fingerprint(&geometry);

        let moved = vec![
            SpineNode::new(Point::new(0.0, 0.0), 1.0),
            SpineNode::new(Point::new(150.0, 90.0), 1.0),
            SpineNode::new(Point::new(300.0, 0.0), 1.0),
        ];
        assert!(geometry.remap(&moved));
        assert_eq!(geometry.element_count(), before, "elements were lost");
        assert_ne!(fingerprint(&geometry), before_shape, "geometry did not move");
    }
}
