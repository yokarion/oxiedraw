//! What a generator emits: either a ribbon (a polyline with a half-width per
//! vertex) or a fill (a closed contour). Both stay in ribbon space until
//! [`map_element`], so geometry re-maps onto an edited curve without
//! regenerating.

use oxiedraw_utils::geometry::Point;

use crate::spine::SpineField;

/// One vertex in ribbon space: `s` along the curve and `n` across it, both in
/// pixels of the curve's rest length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RibbonPoint {
    pub s: f32,
    pub n: f32,
    /// Half the stroke width at this vertex. Ignored for fills.
    pub half_width: f32,
}

impl RibbonPoint {
    pub const fn new(s: f32, n: f32, half_width: f32) -> Self {
        Self { s, n, half_width }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub points: Vec<RibbonPoint>,
    /// Coverage this element contributes, `0..=1`.
    pub alpha: f32,
    /// Paint order. Lower draws first.
    pub depth: i16,
    /// `true` to fill the contour, `false` to stroke the polyline.
    pub fill: bool,
    /// `true` for a stroke that travels back the way it came. Carried so a
    /// preview can color the two apart; the rasteriser does not care.
    pub reverse: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GeometrySink {
    pub elements: Vec<Element>,
    /// Shift across the curve, in ribbon pixels, added to everything pushed in.
    bias: f32,
}

impl GeometrySink {
    pub const fn new() -> Self {
        Self {
            elements: Vec::new(),
            bias: 0.0,
        }
    }

    /// Sit everything this generator emits `n` pixels off the curve, without
    /// every emission site having to know. Occlusion is unaffected: they all
    /// move together.
    pub fn set_bias(&mut self, n: f32) {
        self.bias = n;
    }

    /// Degenerate input is ignored, so generators can push without guarding
    /// every edge case.
    pub fn ribbon(&mut self, points: Vec<RibbonPoint>, alpha: f32, depth: i16) {
        self.stroke(points, alpha, depth, false);
    }

    pub fn stroke(&mut self, mut points: Vec<RibbonPoint>, alpha: f32, depth: i16, reverse: bool) {
        if points.len() < 2 || alpha <= 0.0 {
            return;
        }
        self.shift(&mut points);
        self.elements.push(Element {
            points,
            alpha: alpha.clamp(0.0, 1.0),
            depth,
            fill: false,
            reverse,
        });
    }

    pub fn fill(&mut self, mut points: Vec<RibbonPoint>, alpha: f32, depth: i16) {
        if points.len() < 3 || alpha <= 0.0 {
            return;
        }
        self.shift(&mut points);
        self.elements.push(Element {
            points,
            alpha: alpha.clamp(0.0, 1.0),
            depth,
            fill: true,
            reverse: false,
        });
    }

    fn shift(&self, points: &mut [RibbonPoint]) {
        if self.bias == 0.0 {
            return;
        }
        for point in points {
            point.n += self.bias;
        }
    }

    pub fn len(&self) -> usize {
        self.elements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    pub fn vertex_count(&self) -> usize {
        self.elements.iter().map(|e| e.points.len()).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasVertex {
    pub pos: Point,
    pub half_width: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CanvasElement {
    pub verts: Vec<CanvasVertex>,
    pub alpha: f32,
    pub depth: i16,
    pub fill: bool,
    /// See [`Element::reverse`].
    pub reverse: bool,
}

#[must_use]
pub fn map_element(field: &SpineField, element: &Element) -> CanvasElement {
    CanvasElement {
        verts: element
            .points
            .iter()
            .map(|p| CanvasVertex {
                pos: field.map(p.s, p.n),
                half_width: p.half_width.max(0.0),
            })
            .collect(),
        alpha: element.alpha,
        depth: element.depth,
        fill: element.fill,
        reverse: element.reverse,
    }
}

/// Bounding box `(min_x, min_y, max_x, max_y)`, including each vertex's
/// half-width.
#[must_use]
pub fn canvas_bounds(elements: &[CanvasElement]) -> Option<(f32, f32, f32, f32)> {
    let mut bounds: Option<(f32, f32, f32, f32)> = None;
    for element in elements {
        for v in &element.verts {
            let w = if element.fill { 0.0 } else { v.half_width };
            let (x0, y0, x1, y1) = (v.pos.x - w, v.pos.y - w, v.pos.x + w, v.pos.y + w);
            bounds = Some(bounds.map_or((x0, y0, x1, y1), |b| {
                (b.0.min(x0), b.1.min(y0), b.2.max(x1), b.3.max(y1))
            }));
        }
    }
    bounds
}

/// Whether `(s, n)` falls inside a closed ribbon-space contour, by crossing
/// count.
#[must_use]
pub fn contains(contour: &[RibbonPoint], s: f32, n: f32) -> bool {
    if contour.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = contour.len() - 1;
    for i in 0..contour.len() {
        let (a, b) = (contour[i], contour[j]);
        if (a.n > n) != (b.n > n) {
            let span = b.n - a.n;
            if span.abs() > f32::EPSILON && s < (b.s - a.s) * (n - a.n) / span + a.s {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::spine::{SpineField, SpineNode};

    fn straight_field() -> SpineField {
        let nodes = vec![
            SpineNode::new(Point::new(0.0, 0.0), 1.0),
            SpineNode::new(Point::new(100.0, 0.0), 1.0),
        ];
        SpineField::new(&nodes, 1.0, None).expect("field")
    }

    #[test]
    fn sink_rejects_degenerate_elements() {
        let mut sink = GeometrySink::new();
        sink.ribbon(vec![RibbonPoint::new(0.0, 0.0, 1.0)], 1.0, 0);
        sink.ribbon(Vec::new(), 1.0, 0);
        sink.ribbon(
            vec![RibbonPoint::new(0.0, 0.0, 1.0), RibbonPoint::new(1.0, 1.0, 1.0)],
            0.0,
            0,
        );
        sink.fill(
            vec![RibbonPoint::new(0.0, 0.0, 0.0), RibbonPoint::new(1.0, 1.0, 0.0)],
            1.0,
            0,
        );
        assert!(sink.is_empty());
    }

    #[test]
    fn mapping_puts_ribbon_space_on_the_canvas() {
        let field = straight_field();
        let element = Element {
            points: vec![RibbonPoint::new(50.0, 0.0, 4.0), RibbonPoint::new(50.0, 20.0, 0.0)],
            alpha: 1.0,
            depth: 0,
            fill: false,
            reverse: false,
        };
        let mapped = map_element(&field, &element);
        assert!((mapped.verts[0].pos.x - 50.0).abs() < 0.5);
        assert!(mapped.verts[0].pos.y.abs() < 0.1);
        // The left normal of a rightward stroke points up the screen.
        assert!((mapped.verts[1].pos.y + 20.0).abs() < 0.2);
    }

    #[test]
    fn bounds_account_for_stroke_width() {
        let field = straight_field();
        let element = Element {
            points: vec![RibbonPoint::new(0.0, 0.0, 5.0), RibbonPoint::new(20.0, 0.0, 5.0)],
            alpha: 1.0,
            depth: 0,
            fill: false,
            reverse: false,
        };
        let mapped = vec![map_element(&field, &element)];
        let (x0, y0, x1, _) = canvas_bounds(&mapped).expect("bounds");
        assert!((x0 + 5.0).abs() < 0.5, "x0 {x0}");
        assert!((y0 + 5.0).abs() < 0.5, "y0 {y0}");
        assert!((x1 - 25.0).abs() < 0.5, "x1 {x1}");
        assert!(canvas_bounds(&[]).is_none());
    }
}
