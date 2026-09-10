//! Generative patterns grown along a drawn curve: fur, and the machinery the
//! rest of them will share.
//!
//! Every generator runs the same five stages - fit the curve, frame it, scatter
//! roots along it, grow each root into geometry, rasterise - and only the fourth
//! is pattern-specific. Geometry comes out in ribbon space (`s` along the curve,
//! `n` across it) and randomness is keyed on element ids rather than a stream,
//! so editing the curve re-maps what exists instead of reshuffling it. Pure CPU
//! and deterministic; output is 8-bit coverage ([`CoverageTile`]) the canvas
//! tints with the active color.
//!
//! # Example
//! ```
//! use oxiedraw_patterns::{
//!     Params, PatternRequest, RasterOptions, Side, SpineNode, StrokeStyle,
//! };
//! use oxiedraw_utils::geometry::Point;
//!
//! let pattern = oxiedraw_patterns::by_id("fur").expect("fur is built in");
//! let nodes = vec![
//!     SpineNode::new(Point::new(20.0, 100.0), 0.4),
//!     SpineNode::new(Point::new(220.0, 90.0), 1.0),
//! ];
//! let params = pattern.defaults();
//! let bindings = Default::default();
//! let tile = oxiedraw_patterns::render(
//!     &PatternRequest {
//!         pattern,
//!         params: &params,
//!         bindings: &bindings,
//!         nodes: &nodes,
//!         seed: 1,
//!         side: Side::Left,
//!         // Or `StrokeStyle::Ink { width: 4.0 }` for line art.
//!         style: StrokeStyle::Fill,
//!         rest_length: None,
//!     },
//!     &RasterOptions { canvas: Some((512, 512)), edge: 1.0 },
//! )
//! .expect("fur along a 200 px line");
//! assert!(tile.width > 0 && tile.height > 0);
//! # let _: Params = params;
//! ```

pub mod contour;
pub mod geom;
pub mod noise;
pub mod params;
pub mod patterns;
pub mod raster;
pub mod rng;
pub mod scatter;
pub mod spine;

pub use contour::Segment;
pub use geom::{CanvasElement, CanvasVertex, Element, GeometrySink, RibbonPoint};
pub use params::{
    Bindings, Curve, ModSource, Modulation, ParamDef, ParamKind, ParamValue, Params, Resolved,
};
pub use raster::{CoverageTile, RasterOptions, rasterize};
pub use rng::Rng;
pub use spine::{FrameSample, Side, SpineField, SpineNode};

/// A pattern generator. Implementors are unit structs; `Sync` so the registry
/// can be a static.
pub trait Pattern: Sync {
    /// Persisted in documents. Never rename one.
    fn id(&self) -> &'static str;

    fn label(&self) -> &'static str;

    /// One line for the picker's subtitle.
    fn description(&self) -> &'static str {
        ""
    }

    /// The knobs this generator reads. Drives the whole options panel.
    fn schema(&self) -> &'static [ParamDef];

    fn defaults(&self) -> Params {
        Params::from_schema(self.schema())
    }

    /// Grow the pattern along `ctx.field`, emitting ribbon-space geometry.
    fn generate(&self, ctx: &GenCtx<'_>, out: &mut GeometrySink);
}

/// Whether a stroke comes out as a solid shape or as drawn lines. Ink is
/// generated rather than traced off the silhouette, which can only give a
/// closed outline of even width.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum StrokeStyle {
    #[default]
    Fill,
    /// `width` is the widest the pen gets, in pixels.
    Ink { width: f32 },
}

impl StrokeStyle {
    #[must_use]
    pub fn ink_width(self) -> Option<f32> {
        match self {
            Self::Ink { width } if width > 0.0 => Some(width),
            _ => None,
        }
    }
}

/// What a generator is handed.
pub struct GenCtx<'a> {
    pub field: &'a SpineField,
    /// Knob values resolved against the generator's own schema.
    pub params: Resolved<'a>,
    /// Input bindings (pressure, curvature, ...) per knob.
    pub bindings: &'a Bindings,
    pub style: StrokeStyle,
    pub rng: Rng,
}

impl GenCtx<'_> {
    /// Apply `id`'s input binding to an arbitrary value at `frame`.
    #[must_use]
    pub fn modulate(&self, base: f32, id: &str, frame: &FrameSample) -> f32 {
        self.bindings.get(id).map_or(base, |modulation| {
            self.field.modulate(base, modulation, frame, self.rng.seed())
        })
    }

    /// A float knob with its input binding applied at `frame`. Read knobs
    /// through this, not from `params`, or bindings never take effect.
    #[must_use]
    pub fn value(&self, id: &str, frame: &FrameSample) -> f32 {
        self.modulate(self.params.float(id), id, frame)
    }
}

static FUR: patterns::fur::Fur = patterns::fur::Fur;

/// Every built-in generator, in picker order.
pub static PATTERNS: &[&dyn Pattern] = &[&FUR];

#[must_use]
pub fn by_id(id: &str) -> Option<&'static dyn Pattern> {
    PATTERNS.iter().copied().find(|p| p.id() == id)
}

#[must_use]
pub fn default_pattern() -> &'static dyn Pattern {
    PATTERNS[0]
}

pub struct PatternRequest<'a> {
    pub pattern: &'static dyn Pattern,
    pub params: &'a Params,
    pub bindings: &'a Bindings,
    /// The editable curve, in canvas pixels, with pressure per node.
    pub nodes: &'a [SpineNode],
    pub seed: u64,
    pub side: Side,
    pub style: StrokeStyle,
    /// `None` for a fresh stroke; pass the stored value when regenerating an
    /// edited one so elements keep their place along the curve.
    pub rest_length: Option<f32>,
}

struct GeneratedSide {
    field: SpineField,
    sink: GeometrySink,
}

/// Generated geometry, still in ribbon space. Kept between edits so
/// [`Self::remap`] can move it onto an edited curve without regenerating.
pub struct PatternGeometry {
    sides: Vec<GeneratedSide>,
    rest_length: f32,
}

impl PatternGeometry {
    /// Back ranks first.
    #[must_use]
    pub fn to_canvas(&self) -> Vec<CanvasElement> {
        let mut out: Vec<CanvasElement> = self
            .sides
            .iter()
            .flat_map(|side| {
                side.sink
                    .elements
                    .iter()
                    .map(move |element| geom::map_element(&side.field, element))
            })
            .collect();
        out.sort_by_key(|e| e.depth);
        out
    }

    /// No-op returning `false` if the edited curve is degenerate.
    pub fn remap(&mut self, nodes: &[SpineNode]) -> bool {
        let mut rebuilt = Vec::with_capacity(self.sides.len());
        for side in &self.sides {
            let Some(field) = SpineField::new(nodes, side.field.sign(), Some(self.rest_length))
            else {
                return false;
            };
            rebuilt.push(field);
        }
        for (side, field) in self.sides.iter_mut().zip(rebuilt) {
            side.field = field;
        }
        true
    }

    pub fn element_count(&self) -> usize {
        self.sides.iter().map(|s| s.sink.len()).sum()
    }

    pub fn vertex_count(&self) -> usize {
        self.sides.iter().map(|s| s.sink.vertex_count()).sum()
    }

    /// Persist this with the stroke.
    pub const fn rest_length(&self) -> f32 {
        self.rest_length
    }
}

/// Grow a pattern along a curve. `None` for a curve too short to frame.
#[must_use]
pub fn generate(request: &PatternRequest<'_>) -> Option<PatternGeometry> {
    // Measure the rest length on the framed curve rather than the raw node
    // polyline, so ribbon space and the frame agree to the pixel.
    let rest_length = match request.rest_length.filter(|r| *r > f32::EPSILON) {
        Some(length) => length,
        None => SpineField::new(request.nodes, 1.0, None)?.length(),
    };
    if rest_length <= f32::EPSILON {
        return None;
    }

    let mut sides = Vec::with_capacity(request.side.signs().len());
    for (index, &sign) in request.side.signs().iter().enumerate() {
        let Some(field) = SpineField::new(request.nodes, sign, Some(rest_length)) else {
            continue;
        };
        let mut sink = GeometrySink::new();
        {
            let ctx = GenCtx {
                field: &field,
                params: Resolved::new(request.pattern.schema(), request.params),
                bindings: request.bindings,
                style: request.style,
                // Each side draws its own numbers, or Both would come out
                // mirror-symmetric.
                rng: Rng::new(request.seed).salted(index as u64),
            };
            request.pattern.generate(&ctx, &mut sink);
        }
        sides.push(GeneratedSide { field, sink });
    }
    if sides.is_empty() {
        return None;
    }
    Some(PatternGeometry { sides, rest_length })
}

#[must_use]
pub fn render(request: &PatternRequest<'_>, options: &RasterOptions) -> Option<CoverageTile> {
    let geometry = generate(request)?;
    raster::rasterize(&geometry.to_canvas(), options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxiedraw_utils::geometry::Point;

    fn line() -> Vec<SpineNode> {
        vec![
            SpineNode::new(Point::new(40.0, 120.0), 1.0),
            SpineNode::new(Point::new(240.0, 120.0), 1.0),
        ]
    }

    #[test]
    fn the_registry_is_consistent() {
        assert!(!PATTERNS.is_empty());
        for pattern in PATTERNS {
            assert!(!pattern.id().is_empty());
            assert!(!pattern.label().is_empty());
            assert!(!pattern.schema().is_empty());
            assert_eq!(by_id(pattern.id()).map(Pattern::id), Some(pattern.id()));
        }
        assert!(by_id("no-such-pattern").is_none());
        assert_eq!(default_pattern().id(), PATTERNS[0].id());
    }

    #[test]
    fn defaults_come_from_the_schema() {
        let pattern = default_pattern();
        let params = pattern.defaults();
        assert_eq!(params.len(), pattern.schema().len());
    }

    #[test]
    fn render_produces_a_tile_inside_the_canvas() {
        let pattern = by_id("fur").expect("fur");
        let params = pattern.defaults();
        let bindings = Bindings::default();
        let nodes = line();
        let tile = render(
            &PatternRequest {
                pattern,
                params: &params,
                bindings: &bindings,
                nodes: &nodes,
                seed: 9,
                side: Side::Left,
                style: StrokeStyle::Fill,
                rest_length: None,
            },
            &RasterOptions {
                canvas: Some((512, 512)),
                ..RasterOptions::default()
            },
        )
        .expect("tile");
        assert!(tile.x >= 0 && tile.y >= 0);
        assert!(i64::from(tile.x) + i64::from(tile.width) <= 512);
        assert!(i64::from(tile.y) + i64::from(tile.height) <= 512);
        assert!(tile.data.contains(&255), "nothing is solid");
        assert!(
            tile.data.iter().any(|&v| v > 0 && v < 255),
            "nothing is anti-aliased"
        );
    }

    #[test]
    fn rendering_is_reproducible() {
        let pattern = by_id("fur").expect("fur");
        let params = pattern.defaults();
        let bindings = Bindings::default();
        let nodes = line();
        let request = PatternRequest {
            pattern,
            params: &params,
            bindings: &bindings,
            nodes: &nodes,
            seed: 4,
            side: Side::Both,
            style: StrokeStyle::Fill,
            rest_length: None,
        };
        let options = RasterOptions {
            canvas: Some((512, 512)),
            ..RasterOptions::default()
        };
        let a = render(&request, &options).expect("tile");
        let b = render(&request, &options).expect("tile");
        assert_eq!(a, b);
    }

    fn fur_request<'a>(
        params: &'a Params,
        bindings: &'a Bindings,
        nodes: &'a [SpineNode],
        style: StrokeStyle,
    ) -> PatternRequest<'a> {
        PatternRequest {
            pattern: by_id("fur").expect("fur"),
            params,
            bindings,
            nodes,
            seed: 31,
            side: Side::Left,
            style,
            rest_length: None,
        }
    }

    #[test]
    fn ink_marks_a_fraction_of_what_the_fill_covers() {
        let params = by_id("fur").expect("fur").defaults();
        let bindings = Bindings::default();
        let nodes = line();
        let options = RasterOptions {
            canvas: Some((512, 512)),
            ..RasterOptions::default()
        };
        let filled = render(&fur_request(&params, &bindings, &nodes, StrokeStyle::Fill), &options)
            .expect("filled");
        let inked = render(
            &fur_request(&params, &bindings, &nodes, StrokeStyle::Ink { width: 4.0 }),
            &options,
        )
        .expect("inked");
        let solid = |tile: &CoverageTile| tile.data.iter().filter(|&&v| v > 128).count();
        assert!(
            solid(&inked) < solid(&filled) / 2,
            "ink should cover far less than the fill: {} vs {}",
            solid(&inked),
            solid(&filled)
        );
    }

    #[test]
    fn ink_strokes_taper_along_their_length() {
        let params = by_id("fur").expect("fur").defaults();
        let bindings = Bindings::default();
        let nodes = line();
        let geometry = generate(&fur_request(
            &params,
            &bindings,
            &nodes,
            StrokeStyle::Ink { width: 6.0 },
        ))
        .expect("geometry");
        let strokes = geometry.to_canvas();
        assert!(!strokes.is_empty());
        for stroke in &strokes {
            assert!(!stroke.fill, "ink is stroked, never filled");
            let widest = stroke
                .verts
                .iter()
                .map(|v| v.half_width)
                .fold(0.0_f32, f32::max);
            let thinnest = stroke
                .verts
                .iter()
                .map(|v| v.half_width)
                .fold(f32::INFINITY, f32::min);
            assert!(widest <= 3.0 + 1e-4, "ink exceeded its width: {widest}");
            assert!(
                thinnest < widest * 0.5,
                "stroke barely tapers: {thinnest} to {widest}"
            );
        }
    }

    // ink_detail is off by default: a mark too small or too close to the front
    // line reads as a fleck rather than as fur behind it.
    #[test]
    fn ink_runs_inside_the_mass_and_not_only_around_it() {
        let mut params = by_id("fur").expect("fur").defaults();
        params.set_float("ink_detail", 1.0);
        let bindings = Bindings::default();
        // A bend, not a flat run: on the flat the ranks lie on top of one
        // another and the one behind is suppressed. Walked so the default side
        // is inside the arc, making its surface a bulge.
        let nodes: Vec<SpineNode> = (0..=60_i32)
            .map(|i| {
                let a = std::f32::consts::PI * (i as f32 / 60.0);
                SpineNode::new(
                    Point::new(500.0 + a.cos() * 190.0, 480.0 - a.sin() * 190.0),
                    1.0,
                )
            })
            .collect();
        let options = RasterOptions {
            canvas: Some((1024, 600)),
            ..RasterOptions::default()
        };
        let filled = render(&fur_request(&params, &bindings, &nodes, StrokeStyle::Fill), &options)
            .expect("filled");
        let inked = render(
            &fur_request(&params, &bindings, &nodes, StrokeStyle::Ink { width: 4.0 }),
            &options,
        )
        .expect("inked");

        // Well inside the mass: a whole disc of solid coverage.
        let radius = 5_i32;
        let interior = |x: i32, y: i32| {
            (-radius..=radius).all(|dy| {
                (-radius..=radius).all(|dx| {
                    dx * dx + dy * dy > radius * radius
                        || filled.coverage_at(x + dx, y + dy) == 255
                })
            })
        };
        let mut interior_ink = 0;
        for y in inked.y..inked.y + inked.height as i32 {
            for x in inked.x..inked.x + inked.width as i32 {
                if interior(x, y) && inked.coverage_at(x, y) > 128 {
                    interior_ink += 1;
                }
            }
        }
        assert!(
            interior_ink > 50,
            "only {interior_ink} inked pixels fell inside the mass"
        );
    }

    #[test]
    fn rest_length_is_frozen_at_generation() {
        let pattern = by_id("fur").expect("fur");
        let params = pattern.defaults();
        let bindings = Bindings::default();
        let nodes = line();
        let geometry = generate(&PatternRequest {
            pattern,
            params: &params,
            bindings: &bindings,
            nodes: &nodes,
            seed: 4,
            side: Side::Left,
            style: StrokeStyle::Fill,
            rest_length: None,
        })
        .expect("geometry");
        assert!((geometry.rest_length() - 200.0).abs() < 2.0);

        let stretched = vec![
            SpineNode::new(Point::new(40.0, 120.0), 1.0),
            SpineNode::new(Point::new(440.0, 120.0), 1.0),
        ];
        let mut moved = generate(&PatternRequest {
            pattern,
            params: &params,
            bindings: &bindings,
            nodes: &nodes,
            seed: 4,
            side: Side::Left,
            style: StrokeStyle::Fill,
            rest_length: None,
        })
        .expect("geometry");
        assert!(moved.remap(&stretched));
        let far = moved
            .to_canvas()
            .iter()
            .flat_map(|e| e.verts.iter().map(|v| v.pos.x))
            .fold(0.0_f32, f32::max);
        assert!(far > 400.0, "elements did not follow the stretch: {far}");
    }
}
