//! Per-document state for the Pattern tool.
//!
//! The generator lives in `oxiedraw-patterns` and knows nothing about
//! documents; this is one document's choice of pattern, the values its knobs are
//! set to, and the seed. Shared by clone through `Rc` the way the other tools'
//! state is, so panels and the canvas see the same cells.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use oxiedraw_patterns::{CoverageTile, Params, Side, StrokeStyle};

use crate::color::Color;

/// How the pattern is inked. Mirrors [`StrokeStyle`] but is `Copy` and carries
/// its own pen width, so the UI can flip between the two without losing it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PatternStyle {
    pub line_art: bool,
    pub width: f32,
}

impl Default for PatternStyle {
    fn default() -> Self {
        // The pen width the generator was tuned at. Fill is not offered yet.
        Self { line_art: true, width: 4.5 }
    }
}

impl PatternStyle {
    #[must_use]
    pub const fn to_stroke_style(self) -> StrokeStyle {
        if self.line_art {
            StrokeStyle::Ink { width: self.width }
        } else {
            StrokeStyle::Fill
        }
    }
}

/// Not the curve: that is the tool's own, lives in the UI while the tool is up,
/// and is gone once it bakes.
pub struct PatternState {
    /// Registry id of the active pattern ("fur", ...).
    pub pattern_id: Rc<RefCell<String>>,
    /// Kept per pattern id, so switching away and back does not throw away what
    /// was set.
    params: Rc<RefCell<HashMap<String, Params>>>,
    pub style: Rc<RefCell<PatternStyle>>,
    /// Re-rolled per stroke, so two strokes with the same settings still differ.
    pub seed: Rc<Cell<u64>>,
}

impl std::fmt::Debug for PatternState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatternState")
            .field("pattern_id", &self.pattern_id.borrow())
            .field("seed", &self.seed.get())
            .finish_non_exhaustive()
    }
}

impl Clone for PatternState {
    fn clone(&self) -> Self {
        Self {
            pattern_id: Rc::clone(&self.pattern_id),
            params: Rc::clone(&self.params),
            style: Rc::clone(&self.style),
            seed: Rc::clone(&self.seed),
        }
    }
}

impl PatternState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pattern_id: Rc::new(RefCell::new(
                oxiedraw_patterns::default_pattern().id().to_string(),
            )),
            params: Rc::new(RefCell::new(HashMap::new())),
            style: Rc::new(RefCell::new(PatternStyle::default())),
            seed: Rc::new(Cell::new(1)),
        }
    }

    /// Falls back to the registry default if the stored id no longer exists.
    #[must_use]
    pub fn pattern(&self) -> &'static dyn oxiedraw_patterns::Pattern {
        oxiedraw_patterns::by_id(&self.pattern_id.borrow())
            .unwrap_or_else(oxiedraw_patterns::default_pattern)
    }

    /// Seeded from the schema defaults the first time it is asked for.
    #[must_use]
    pub fn params(&self) -> Params {
        let pattern = self.pattern();
        let id = pattern.id().to_string();
        self.params
            .borrow_mut()
            .entry(id)
            .or_insert_with(|| pattern.defaults())
            .clone()
    }

    pub fn set_params(&self, params: Params) {
        let id = self.pattern().id().to_string();
        self.params.borrow_mut().insert(id, params);
    }

    /// Resolved against the active pattern's schema, so callers read them the
    /// way a generator does.
    pub fn resolved<'a>(&self, params: &'a Params) -> oxiedraw_patterns::Resolved<'a> {
        oxiedraw_patterns::Resolved::new(self.pattern().schema(), params)
    }

    /// Which side of the line the pattern grows on. A schema bool rather than
    /// its own cell, so the panel needs no control of its own for it.
    #[must_use]
    pub fn side_for(&self, params: &Params) -> Side {
        if self.resolved(params).bool("flip_side") {
            Side::Right
        } else {
            Side::Left
        }
    }

    /// Deterministic given the previous one, so a document replays identically.
    pub fn advance_seed(&self) -> u64 {
        let next = oxiedraw_patterns::rng::mix64(self.seed.get().wrapping_add(0x9E37_79B9_7F4A_7C15));
        self.seed.set(next);
        next
    }
}

impl Default for PatternState {
    fn default() -> Self {
        Self::new()
    }
}

/// Composite a generated coverage tile onto layer pixels in one flat colour, on
/// the terms a brush stamp uses: premultiplied sRGB BGRA, source-over, clipped
/// by the selection mask. `layer` is canvas-sized and edited in place.
pub fn paint_coverage(
    layer: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    tile: &CoverageTile,
    color: Color,
    opacity: f32,
    selection: Option<&[u8]>,
) {
    let (canvas_w, canvas_h) = (canvas_w as i32, canvas_h as i32);
    let opacity = opacity.clamp(0.0, 1.0);
    if opacity <= 0.0 {
        return;
    }
    let (cr, cg, cb) = (u32::from(color.r), u32::from(color.g), u32::from(color.b));

    let x_start = tile.x.max(0);
    let y_start = tile.y.max(0);
    let x_end = (tile.x + tile.width as i32).min(canvas_w);
    let y_end = (tile.y + tile.height as i32).min(canvas_h);

    for py in y_start..y_end {
        for px in x_start..x_end {
            let mut a = f32::from(tile.coverage_at(px, py)) * opacity;
            if a <= 0.0 {
                continue;
            }
            let index = (py * canvas_w + px) as usize;
            if let Some(mask) = selection {
                let Some(&gate) = mask.get(index) else { continue };
                if gate == 0 {
                    continue;
                }
                a = a * f32::from(gate) / 255.0;
            }
            let a = a.round() as u32;
            if a == 0 {
                continue;
            }
            let idx = index * 4;
            let inv = 255 - a;
            // Source is premultiplied by its own coverage before the OVER.
            let sb = (cb * a + 127) / 255;
            let sg = (cg * a + 127) / 255;
            let sr = (cr * a + 127) / 255;
            layer[idx] = (sb + (u32::from(layer[idx]) * inv + 127) / 255) as u8;
            layer[idx + 1] = (sg + (u32::from(layer[idx + 1]) * inv + 127) / 255) as u8;
            layer[idx + 2] = (sr + (u32::from(layer[idx + 2]) * inv + 127) / 255) as u8;
            layer[idx + 3] = (a + (u32::from(layer[idx + 3]) * inv + 127) / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_are_kept_per_pattern() {
        let state = PatternState::new();
        let mut params = state.params();
        params.set_float("length", 123.0);
        state.set_params(params);
        assert_eq!(
            state.params().get("length"),
            Some(oxiedraw_patterns::ParamValue::Float(123.0))
        );

        // An unknown id falls back to the default pattern rather than panicking,
        // so a project naming a generator we no longer ship still opens.
        *state.pattern_id.borrow_mut() = "no-such-pattern".to_string();
        assert_eq!(state.pattern().id(), oxiedraw_patterns::default_pattern().id());
    }

    // What the panel's reset button does. It is a whole-map replace, not a
    // per-knob rewind, so a stale id cannot survive with no control to show it.
    #[test]
    fn resetting_returns_every_knob_to_the_schema() {
        let state = PatternState::new();
        let fresh = state.params();
        let fresh_side = state.side_for(&fresh);

        let mut edited = fresh.clone();
        edited.set_float("length", 999.0);
        // Whichever side the schema does not ship: that default moves.
        edited.set_bool("flip_side", fresh_side == Side::Left);
        edited.set_float("nothing_defines_this", 1.0);
        state.set_params(edited);
        assert_ne!(state.params(), fresh);
        assert_ne!(state.side_for(&state.params()), fresh_side);

        state.set_params(state.pattern().defaults());
        assert_eq!(state.params(), fresh);
        assert_eq!(state.params().get("nothing_defines_this"), None);
        assert_eq!(state.side_for(&state.params()), fresh_side);
    }

    #[test]
    fn seeds_advance_and_do_not_repeat() {
        let state = PatternState::new();
        let first = state.advance_seed();
        let second = state.advance_seed();
        assert_ne!(first, second);
        assert_eq!(state.seed.get(), second);
    }

    fn fur_tile(w: u32, h: u32) -> CoverageTile {
        use oxiedraw_patterns::{Bindings, PatternRequest, RasterOptions, SpineNode};
        let state = PatternState::new();
        let params = state.params();
        let bindings = Bindings::default();
        let nodes = vec![
            SpineNode::new(oxiedraw_utils::geometry::Point::new(20.0, 60.0), 1.0),
            SpineNode::new(oxiedraw_utils::geometry::Point::new(w as f32 - 20.0, 60.0), 1.0),
        ];
        oxiedraw_patterns::render(
            &PatternRequest {
                pattern: state.pattern(),
                params: &params,
                bindings: &bindings,
                nodes: &nodes,
                seed: 7,
                side: state.side_for(&params),
                style: state.style.borrow().to_stroke_style(),
                rest_length: None,
            },
            &RasterOptions { canvas: Some((w, h)), edge: 1.0 },
        )
        .expect("fur generated")
    }

    #[test]
    fn coverage_lands_as_premultiplied_paint_in_the_active_colour() {
        let (w, h) = (200_u32, 140_u32);
        let tile = fur_tile(w, h);
        let mut layer = vec![0_u8; (w * h * 4) as usize];
        paint_coverage(&mut layer, w, h, &tile, Color::new(255, 0, 0), 1.0, None);

        let painted: Vec<usize> = (0..(w * h) as usize)
            .filter(|i| layer[i * 4 + 3] > 0)
            .collect();
        assert!(!painted.is_empty(), "nothing was painted");
        for i in painted {
            let (b, g, r, a) = (
                layer[i * 4],
                layer[i * 4 + 1],
                layer[i * 4 + 2],
                layer[i * 4 + 3],
            );
            assert_eq!(b, 0, "blue leaked into a red pattern");
            assert_eq!(g, 0, "green leaked into a red pattern");
            assert!(
                r <= a,
                "not premultiplied: colour {r} exceeds coverage {a} at {i}"
            );
        }
    }

    // The mask is the same canvas-sized R8 buffer the fill tool clips against.
    #[test]
    fn a_selection_clips_the_pattern() {
        let (w, h) = (200_u32, 140_u32);
        let tile = fur_tile(w, h);
        let mut open = vec![0_u8; (w * h * 4) as usize];
        paint_coverage(&mut open, w, h, &tile, Color::new(255, 0, 0), 1.0, None);

        // Let only the left half through.
        let mut mask = vec![0_u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w / 2 {
                mask[(y * w + x) as usize] = 255;
            }
        }
        let mut clipped = vec![0_u8; (w * h * 4) as usize];
        paint_coverage(
            &mut clipped,
            w,
            h,
            &tile,
            Color::new(255, 0, 0),
            1.0,
            Some(&mask),
        );

        let right_open = (0..(w * h) as usize)
            .filter(|i| (*i as u32 % w) >= w / 2 && open[i * 4 + 3] > 0)
            .count();
        let right_clipped = (0..(w * h) as usize)
            .filter(|i| (*i as u32 % w) >= w / 2 && clipped[i * 4 + 3] > 0)
            .count();
        assert!(right_open > 0, "the unclipped run never reached the right");
        assert_eq!(right_clipped, 0, "the mask did not hold the pattern back");
    }
}
