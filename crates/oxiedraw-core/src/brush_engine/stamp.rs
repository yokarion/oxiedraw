use std::collections::VecDeque;

use oxiedraw_utils::geometry::Point;

use crate::color::Color;

use super::dynamics::{Dynamics, evaluate, make_spawn_input};
use super::{
    BrushFamily, BrushPreset, Dab, InputSample, PaintMode, PaintTarget, StrokeContext,
    StrokeRenderer,
};

const MIN_DAB_RADIUS: f32 = 0.5;

/// Krita's floor on the airbrush interval (`MIN_TIMED_INTERVAL`).
const MIN_TIMED_INTERVAL_MS: f32 = 0.5;

/// Segment end with slack for summed fractions, so a dab due right at the end
/// isn't lost to rounding.
const SEGMENT_END: f32 = 1.0 + 1e-5;

/// Milliseconds between airbrush dabs at `rate` dabs per second.
pub(super) fn airbrush_interval_ms(rate: f32) -> f32 {
    (1000.0 / rate.max(f32::EPSILON)).max(MIN_TIMED_INTERVAL_MS)
}

/// Krita's `KisPainter::blendAverageOpacity`: jump up to a higher opacity at
/// once, ease down toward a lower one.
fn blend_average_opacity(opacity: f32, average: f32) -> f32 {
    if average < opacity {
        opacity
    } else {
        0.1f32.mul_add(opacity, 0.9 * average)
    }
}

/// Sample interval the preset EMA factors are tuned against. Motion event
/// rate swings with hardware (GDK coalesces motion to the frame clock when
/// the loop is busy), so per-sample factors are rescaled to this interval -
/// otherwise the same preset lags several times harder on a slow machine.
const EMA_REFERENCE_MS: f32 = 8.0;

/// Rescale a per-sample EMA retention factor to the actual sample interval,
/// so the filter's time constant stays fixed regardless of event rate.
fn rate_adjusted_retention(retention: f32, dt_ms: f32) -> f32 {
    if dt_ms <= f32::EPSILON {
        return retention;
    }
    retention.powf(dt_ms / EMA_REFERENCE_MS)
}

/// Build a renderer for the given preset + stroke context. The single
/// entry point for starting a stroke; replaces the per-brush
/// `start_stroke` method.
pub fn start_stroke(preset: &BrushPreset, ctx: StrokeContext) -> Box<dyn StrokeRenderer> {
    let mut renderer = PresetStrokeRenderer::new(
        ctx,
        preset.family.clone(),
        preset.spacing_ratio,
        preset.stabilizer,
        preset.speed_smoothing,
        preset.dynamics.clone(),
        DabStyle::from_preset(preset),
    );
    renderer.airbrush_interval_ms = preset
        .airbrush
        .then(|| airbrush_interval_ms(preset.airbrush_rate));
    Box::new(renderer)
}

/// Per-stroke tip + grain settings copied onto every dab so the GPU
/// shaders can shape the footprint and modulate it with the global
/// canvas-space texture. Constant across a stroke.
#[derive(Debug, Clone, Copy)]
pub(super) struct DabStyle {
    hardness: f32,
    tip: f32,
    texture_scale: f32,
    texture_strength: f32,
    texturing_mode: f32,
    texture_gain: f32,
    texture_bias: f32,
    wash: bool,
}

impl DabStyle {
    fn from_preset(preset: &BrushPreset) -> Self {
        let (texture_gain, texture_bias) = preset.texture_adjust.gain_bias();
        Self {
            hardness: preset.hardness,
            tip: preset.tip.as_gpu(),
            texture_scale: preset.texture_scale,
            texture_strength: preset.texture_strength,
            texturing_mode: preset.texturing_mode.as_gpu(),
            texture_gain,
            texture_bias,
            // The renderer washes only the soft-round tip; others fall back.
            wash: preset.paint_mode == PaintMode::Wash
                && matches!(preset.family, BrushFamily::SoftRound),
        }
    }

    fn apply(self, dab: &mut Dab) {
        dab.hardness = self.hardness;
        dab.tip = self.tip;
        dab.texture_scale = self.texture_scale;
        dab.texture_strength = self.texture_strength;
        dab.texturing_mode = self.texturing_mode;
        dab.texture_gain = self.texture_gain;
        dab.texture_bias = self.texture_bias;
    }
}

/// Streams input through a uniform Catmull-Rom spline so the painted
/// curve is smooth even when motion events are sparse, then stamps
/// overlapping dabs at `size * spacing` step. Pressure is interpolated
/// along each segment, then handed to the `Dynamics` evaluator before
/// the dab leaves the CPU.
pub(super) struct PresetStrokeRenderer {
    ctx: StrokeContext,
    family: BrushFamily,
    spacing_ratio: f32,
    stabilizer: f32,
    /// EMA smoothing factor for the speed signal (0.0..=1.0 from preset).
    speed_smoothing: f32,
    dynamics: Dynamics,
    style: DabStyle,
    history: VecDeque<InputSample>,
    dabs: Vec<Dab>,
    total_distance: f32,
    /// Distance painted since the last dab actually fired. Carried
    /// across segments so spacing is enforced over arc length, not per
    /// motion event - a slow drag with spacing=1.0 still waits until
    /// the cursor has moved one full diameter before stamping again.
    dist_since_last_dab: f32,
    /// Airbrush counterpart of `dist_since_last_dab`, in milliseconds.
    time_since_last_dab: f32,
    airbrush_interval_ms: Option<f32>,
    average_opacity: f32,
    /// EMA-smoothed speed. Negative = uninitialized (first segment seeds it).
    smoothed_speed: f32,
    rng_state: u32,
}

impl PresetStrokeRenderer {
    pub(super) fn new(
        ctx: StrokeContext,
        family: BrushFamily,
        spacing_ratio: f32,
        stabilizer: f32,
        speed_smoothing: f32,
        dynamics: Dynamics,
        style: DabStyle,
    ) -> Self {
        // Seed the per-stroke RNG so scatter / random dynamics are
        // deterministic per stroke (handy for tests) but distinct
        // between strokes.
        let rng_state = ctx
            .preset
            .0
            .wrapping_mul(0x9e37_79b9)
            .wrapping_add(0x0001_0000)
            .max(1);
        Self {
            ctx,
            family,
            spacing_ratio,
            stabilizer: stabilizer.clamp(0.0, 0.95),
            speed_smoothing: speed_smoothing.clamp(0.0, 1.0),
            dynamics,
            style,
            history: VecDeque::with_capacity(4),
            dabs: Vec::new(),
            total_distance: 0.0,
            dist_since_last_dab: 0.0,
            time_since_last_dab: 0.0,
            airbrush_interval_ms: None,
            average_opacity: 0.0,
            smoothed_speed: -1.0,
            rng_state,
        }
    }

    fn stabilize(&self, sample: InputSample) -> InputSample {
        if self.stabilizer <= f32::EPSILON {
            return sample;
        }
        let Some(prev) = self.history.back().copied() else {
            return sample;
        };
        #[allow(clippy::cast_precision_loss)]
        let dt_ms = (sample.time_ms.saturating_sub(prev.time_ms)) as f32;
        let a = rate_adjusted_retention(self.stabilizer, dt_ms);
        let inv = 1.0 - a;
        InputSample {
            position: Point::new(
                prev.position.x.mul_add(a, sample.position.x * inv),
                prev.position.y.mul_add(a, sample.position.y * inv),
            ),
            pressure: prev.pressure.mul_add(a, sample.pressure * inv),
            tilt_x: sample.tilt_x,
            tilt_y: sample.tilt_y,
            rotation: sample.rotation,
            time_ms: sample.time_ms,
        }
    }

    /// Advance the xorshift32 stream and return three uniformly-
    /// distributed `0..1` floats. Used for scatter seed + random source.
    fn next_random_triple(&mut self) -> (f32, f32, f32) {
        let a = u32_to_unit(self.advance_rng());
        let b = u32_to_unit(self.advance_rng());
        let c = u32_to_unit(self.advance_rng());
        (a, b, c)
    }

    const fn advance_rng(&mut self) -> u32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng_state = x;
        x
    }

    /// Build one dab from the spawn-point info plus any active dynamics.
    /// `segment_speed_px_ms` and `segment_direction_rad` are constant
    /// across the segment, so they're computed once by `paint_segment`
    /// and passed in. `pen_rotation_rad`, `tilt_x`, `tilt_y` are the
    /// tablet axis readings at the current sample (also segment-stable).
    #[allow(clippy::too_many_arguments)]
    fn emit_dab(
        &mut self,
        pos: Point,
        pressure: f32,
        segment_speed_px_ms: f32,
        segment_direction_rad: f32,
        pen_rotation_rad: f32,
        tilt_x: f32,
        tilt_y: f32,
    ) -> Dab {
        let mut dab = Dab::round(pos, self.ctx.size * 0.5, self.ctx.color);
        self.style.apply(&mut dab);
        // Pixel family deliberately skips dynamics - the curves don't
        // line up with the integer grid. Empty `Dynamics` also skips
        // for the cheap path.
        if !matches!(self.family, BrushFamily::Pixel) && self.dynamics.any_active() {
            let (random, scatter_x, scatter_y) = self.next_random_triple();
            let input = make_spawn_input(
                pressure,
                segment_speed_px_ms,
                segment_direction_rad,
                self.total_distance,
                self.ctx.size,
                random,
                pen_rotation_rad,
                tilt_x,
                tilt_y,
            );
            evaluate(
                &self.dynamics,
                &input,
                self.ctx.size,
                (scatter_x, scatter_y),
                &mut dab,
            );
            dab.radius = dab.radius.max(MIN_DAB_RADIUS);
        }
        if self.style.wash {
            self.average_opacity = blend_average_opacity(dab.opacity, self.average_opacity);
            dab.average_opacity = self.average_opacity;
        } else {
            fold_opacity_into_flow(&mut dab);
        }
        dab
    }

    fn paint_segment(
        &mut self,
        target: &mut dyn PaintTarget,
        p0: InputSample,
        p1: InputSample,
        p2: InputSample,
        p3: InputSample,
    ) {
        let dx = p2.position.x - p1.position.x;
        let dy = p2.position.y - p1.position.y;
        let chord = dx.hypot(dy);
        let direction = dy.atan2(dx);
        #[allow(clippy::cast_precision_loss)]
        let dt_ms = (p2.time_ms.saturating_sub(p1.time_ms)) as f32;
        let raw_speed = if dt_ms > f32::EPSILON {
            chord / dt_ms
        } else {
            0.0
        };
        // EMA smoothing: alpha 0.35 (baseline) -> 0.05 (max), clamping
        // out lag-induced spikes without losing real speed variation.
        let alpha = 1.0
            - rate_adjusted_retention(
                1.0 - 0.30f32.mul_add(-self.speed_smoothing, 0.35),
                dt_ms,
            );
        let speed_px_ms = if self.smoothed_speed < 0.0 {
            self.smoothed_speed = raw_speed;
            raw_speed
        } else {
            self.smoothed_speed = alpha.mul_add(raw_speed - self.smoothed_speed, self.smoothed_speed);
            self.smoothed_speed
        };

        let spacing_ratio = self.resolve_spacing_ratio(&p2, speed_px_ms, direction);
        // Spacing tracks the pressure-scaled dab size, not the base size,
        // so a low-pressure (small) dab still overlaps its neighbour rather
        // than leaving isolated dark centres (very visible at large sizes).
        // Floored at 1 px to avoid divide-by-zero.
        let effective_size = self.effective_diameter(&p1, &p2, speed_px_ms, direction);
        let step = (effective_size * spacing_ratio).max(MIN_DAB_RADIUS * 2.0);

        self.dabs.clear();

        // Walk along this segment. Like Krita's `KisDistanceInformation`, a
        // dab fires at whichever comes first - `step` of travel or, for an
        // airbrush, the timing interval - and both counters restart there.
        // The leftovers carry into the next segment.
        let interval = self.airbrush_interval_ms.filter(|_| dt_ms > f32::EPSILON);
        let mut t_prev = 0.0f32;
        loop {
            let by_distance = (chord > f32::EPSILON)
                .then(|| t_prev + (step - self.dist_since_last_dab) / chord)
                .filter(|&t| t <= SEGMENT_END);
            let by_time = interval
                .map(|ms| t_prev + (ms - self.time_since_last_dab) / dt_ms)
                .filter(|&t| t <= SEGMENT_END);
            let t = match (by_distance, by_time) {
                (Some(d), Some(tm)) => d.min(tm),
                (Some(t), None) | (None, Some(t)) => t,
                (None, None) => break,
            };
            // A held-still pen has no spline to follow; stay on the point.
            let pos = if chord > f32::EPSILON {
                catmull_rom(p0.position, p1.position, p2.position, p3.position, t)
            } else {
                p1.position
            };
            let pressure = (p2.pressure - p1.pressure).mul_add(t, p1.pressure);
            let dab = self.emit_dab(
                pos,
                pressure,
                speed_px_ms,
                direction,
                p2.rotation,
                p2.tilt_x,
                p2.tilt_y,
            );
            self.dabs.push(dab);
            self.total_distance += chord.mul_add(t - t_prev, self.dist_since_last_dab);
            self.dist_since_last_dab = 0.0;
            self.time_since_last_dab = 0.0;
            t_prev = t;
        }
        self.dist_since_last_dab += chord * (1.0 - t_prev).max(0.0);
        self.time_since_last_dab += dt_ms * (1.0 - t_prev).max(0.0);

        if !self.dabs.is_empty() {
            target.paint_segment_dabs(&self.dabs, p1.time_ms, p2.time_ms);
        }
    }

    fn paint_initial_dab(&mut self, sample: InputSample, target: &mut dyn PaintTarget) {
        self.dabs.clear();
        let dab = self.emit_dab(
            sample.position,
            sample.pressure,
            0.0,
            0.0,
            sample.rotation,
            sample.tilt_x,
            sample.tilt_y,
        );
        self.dabs.push(dab);
        target.paint_dabs(&self.dabs);
    }

    /// Smallest dab diameter the size dynamics produce across the segment
    /// (evaluated at both endpoints; pressure interpolates monotonically).
    /// Spacing scales by this so even the smallest dab overlaps. Falls back
    /// to the base size when no size mapping is active.
    fn effective_diameter(
        &self,
        p1: &InputSample,
        p2: &InputSample,
        speed_px_ms: f32,
        direction_rad: f32,
    ) -> f32 {
        let Some(mapping) = &self.dynamics.size else {
            return self.ctx.size;
        };
        let diameter_at = |p: &InputSample| {
            let input = make_spawn_input(
                p.pressure,
                speed_px_ms,
                direction_rad,
                self.total_distance,
                self.ctx.size,
                0.0,
                p.rotation,
                p.tilt_x,
                p.tilt_y,
            );
            self.ctx.size * mapping.apply(&input)
        };
        diameter_at(p1).min(diameter_at(p2)).max(MIN_DAB_RADIUS * 2.0)
    }

    /// Compute the spacing ratio for the upcoming segment. With no active
    /// spacing dynamics this is the preset's static ratio; with a mapping
    /// it's evaluated against the same `SpawnInput` the dab dynamics will
    /// see, then floored so the renderer never stamps millions of dabs.
    fn resolve_spacing_ratio(
        &self,
        p2: &InputSample,
        speed_px_ms: f32,
        direction_rad: f32,
    ) -> f32 {
        let Some(mapping) = &self.dynamics.spacing else {
            return self.spacing_ratio;
        };
        // The spacing mapping is sampled once per segment; reuse the
        // segment-stable inputs (pressure / rotation / tilt at p2) and
        // pass `random = 0` so the result is deterministic across the
        // dabs of one segment.
        let input = make_spawn_input(
            p2.pressure,
            speed_px_ms,
            direction_rad,
            self.total_distance,
            self.ctx.size,
            0.0,
            p2.rotation,
            p2.tilt_x,
            p2.tilt_y,
        );
        mapping.apply(&input)
    }
}

impl StrokeRenderer for PresetStrokeRenderer {
    fn push(&mut self, sample: InputSample, target: &mut dyn PaintTarget) {
        target.set_family(&self.family);
        let stabilized = self.stabilize(sample);
        if self.history.is_empty() {
            self.paint_initial_dab(stabilized, target);
        }
        self.history.push_back(stabilized);

        match self.history.len() {
            3 => {
                let s0 = self.history[0];
                let s1 = self.history[1];
                let s2 = self.history[2];
                let p0 = reflect(s0, s1);
                self.paint_segment(target, p0, s0, s1, s2);
            }
            n if n >= 4 => {
                let p0 = self.history[0];
                let p1 = self.history[1];
                let p2 = self.history[2];
                let p3 = self.history[3];
                self.paint_segment(target, p0, p1, p2, p3);
                self.history.pop_front();
            }
            _ => {}
        }
    }

    fn end(&mut self, target: &mut dyn PaintTarget) {
        target.set_family(&self.family);
        match self.history.len() {
            2 => {
                let s0 = self.history[0];
                let s1 = self.history[1];
                let p0 = reflect(s0, s1);
                let p3 = reflect(s1, s0);
                self.paint_segment(target, p0, s0, s1, p3);
            }
            3 => {
                let s0 = self.history[0];
                let s1 = self.history[1];
                let s2 = self.history[2];
                let p3 = reflect(s2, s1);
                self.paint_segment(target, s0, s1, s2, p3);
            }
            _ => {}
        }
        self.history.clear();
    }

    fn preview(&self, target: &mut dyn PaintTarget) {
        target.set_family(&self.family);
        let n = self.history.len();
        if n < 2 {
            return;
        }
        let p1 = self.history[n - 2];
        let p2 = self.history[n - 1];
        let p0 = if n >= 3 {
            self.history[n - 3]
        } else {
            reflect(p1, p2)
        };
        let p3 = reflect(p2, p1);
        // Preview is read-only on `&self`, so we sample dynamics with a
        // *snapshot* of stroke state - distance/rng aren't advanced.
        // Random scatter therefore lands at a stable position frame to
        // frame, which is what we want for the cursor tail.
        let mut dabs = Vec::new();
        emit_segment_dabs_preview(
            self,
            p0,
            p1,
            p2,
            p3,
            self.ctx.color,
            self.ctx.size,
            self.spacing_ratio,
            &mut dabs,
        );
        target.paint_dabs(&dabs);
    }
}

/// Stateless preview emitter. Mirrors `paint_segment` but does not
/// advance stroke state. Used by `StrokeRenderer::preview` which takes
/// `&self`.
#[allow(clippy::too_many_arguments)]
fn emit_segment_dabs_preview(
    renderer: &PresetStrokeRenderer,
    p0: InputSample,
    p1: InputSample,
    p2: InputSample,
    p3: InputSample,
    color: Color,
    base_size: f32,
    spacing_ratio: f32,
    out: &mut Vec<Dab>,
) {
    let dx = p2.position.x - p1.position.x;
    let dy = p2.position.y - p1.position.y;
    let chord = dx.hypot(dy);
    let direction = dy.atan2(dx);
    #[allow(clippy::cast_precision_loss)]
    let dt_ms = (p2.time_ms.saturating_sub(p1.time_ms)) as f32;
    let speed_px_ms = if dt_ms > f32::EPSILON {
        chord / dt_ms
    } else {
        0.0
    };

    let effective_spacing = renderer.dynamics.spacing.as_ref().map_or(spacing_ratio, |mapping| {
        let probe = make_spawn_input(
            p2.pressure,
            speed_px_ms,
            direction,
            renderer.total_distance,
            base_size,
            0.0,
            p2.rotation,
            p2.tilt_x,
            p2.tilt_y,
        );
        mapping.apply(&probe)
    });
    // Mirror the live path: spacing tracks the smallest pressure-scaled
    // dab size across the segment so ramping pressure doesn't gap.
    let effective_size = renderer.dynamics.size.as_ref().map_or(base_size, |mapping| {
        let diameter_at = |p: &InputSample| {
            let probe = make_spawn_input(
                p.pressure,
                speed_px_ms,
                direction,
                renderer.total_distance,
                base_size,
                0.0,
                p.rotation,
                p.tilt_x,
                p.tilt_y,
            );
            base_size * mapping.apply(&probe)
        };
        diameter_at(&p1).min(diameter_at(&p2))
    });
    let step = (effective_size * effective_spacing).max(MIN_DAB_RADIUS * 2.0);

    // Preview is read-only - walk distances locally; no state advance.
    let mut s = step - renderer.dist_since_last_dab;
    while s <= chord {
        let t = s / chord;
        let pos = catmull_rom(p0.position, p1.position, p2.position, p3.position, t);
        let pressure = (p2.pressure - p1.pressure).mul_add(t, p1.pressure);
        let mut dab = Dab::round(pos, base_size * 0.5, color);
        renderer.style.apply(&mut dab);
        if !matches!(renderer.family, BrushFamily::Pixel) && renderer.dynamics.any_active() {
            // Use a stable per-position pseudo-random so the preview
            // tail doesn't visually jitter between frames.
            let stable = (pos.x.to_bits() ^ pos.y.to_bits()).wrapping_mul(0x9e37_79b9);
            let rand = u32_to_unit(stable);
            let input = make_spawn_input(
                pressure,
                speed_px_ms,
                direction,
                renderer.total_distance,
                base_size,
                rand,
                p2.rotation,
                p2.tilt_x,
                p2.tilt_y,
            );
            evaluate(&renderer.dynamics, &input, base_size, (rand, rand), &mut dab);
            dab.radius = dab.radius.max(MIN_DAB_RADIUS);
        }
        if renderer.style.wash {
            dab.average_opacity = blend_average_opacity(dab.opacity, renderer.average_opacity);
        } else {
            fold_opacity_into_flow(&mut dab);
        }
        out.push(dab);
        s += step;
    }
}

/// Without a wash ceiling, opacity is just more coverage attenuation.
fn fold_opacity_into_flow(dab: &mut Dab) {
    dab.flow *= dab.opacity;
    dab.opacity = 1.0;
}

fn u32_to_unit(x: u32) -> f32 {
    // Take the top 24 bits as a mantissa; gives values in [0, 1).
    #[allow(clippy::cast_precision_loss)]
    let v = (x >> 8) as f32;
    v / ((1u32 << 24) as f32)
}

fn reflect(p: InputSample, axis: InputSample) -> InputSample {
    InputSample {
        position: Point::new(
            2.0f32.mul_add(p.position.x, -axis.position.x),
            2.0f32.mul_add(p.position.y, -axis.position.y),
        ),
        pressure: p.pressure,
        tilt_x: p.tilt_x,
        tilt_y: p.tilt_y,
        rotation: p.rotation,
        time_ms: p.time_ms,
    }
}

#[allow(clippy::suboptimal_flops)]
fn catmull_rom(p0: Point, p1: Point, p2: Point, p3: Point, t: f32) -> Point {
    let t2 = t * t;
    let t3 = t2 * t;
    let cx = 2.0 * p1.x
        + (-p0.x + p2.x) * t
        + (2.0 * p0.x - 5.0 * p1.x + 4.0 * p2.x - p3.x) * t2
        + (-p0.x + 3.0 * p1.x - 3.0 * p2.x + p3.x) * t3;
    let cy = 2.0 * p1.y
        + (-p0.y + p2.y) * t
        + (2.0 * p0.y - 5.0 * p1.y + 4.0 * p2.y - p3.y) * t2
        + (-p0.y + 3.0 * p1.y - 3.0 * p2.y + p3.y) * t3;
    Point::new(0.5 * cx, 0.5 * cy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brush_engine::BrushPresetId;

    #[derive(Default)]
    struct Recorder {
        dabs: Vec<Dab>,
    }

    impl PaintTarget for Recorder {
        fn set_family(&mut self, _family: &BrushFamily) {}
        fn paint_dabs(&mut self, dabs: &[Dab]) {
            self.dabs.extend_from_slice(dabs);
        }
    }

    fn sample(x: f32, time_ms: u64, pressure: f32) -> InputSample {
        InputSample {
            position: Point::new(x, 50.0),
            pressure,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation: 0.0,
            time_ms,
        }
    }

    fn stroke(preset: &BrushPreset, samples: &[InputSample]) -> Vec<Dab> {
        let ctx = StrokeContext {
            preset: BrushPresetId(0),
            color: Color::BLACK,
            size: 40.0,
            opacity: 1.0,
        };
        let mut renderer = start_stroke(preset, ctx);
        let mut target = Recorder::default();
        for &s in samples {
            renderer.push(s, &mut target);
        }
        renderer.end(&mut target);
        target.dabs
    }

    fn airbrushing() -> BrushPreset {
        let mut preset = BrushPreset::airbrush_soft(BrushPresetId(0));
        preset.airbrush = true;
        preset.airbrush_rate = 1000.0;
        preset
    }

    #[test]
    fn airbrush_keeps_spraying_on_a_resting_pen() {
        let dabs = stroke(&airbrushing(), &[sample(50.0, 0, 1.0), sample(50.0, 10, 1.0)]);
        // The pen-down dab plus one per millisecond at 1000 dabs a second.
        assert_eq!(dabs.len(), 11);
        assert!(dabs.iter().all(|d| (d.center.x - 50.0).abs() < 1e-4));
    }

    #[test]
    fn without_airbrush_a_resting_pen_lays_nothing_more() {
        let preset = BrushPreset::airbrush_soft(BrushPresetId(0));
        let dabs = stroke(&preset, &[sample(50.0, 0, 1.0), sample(50.0, 10, 1.0)]);
        assert_eq!(dabs.len(), 1);
    }

    #[test]
    fn airbrush_fires_on_time_before_the_spacing_distance() {
        // 4 px in 10 ms: spacing (0.1 x 40 px) allows one dab, time ten.
        let dabs = stroke(&airbrushing(), &[sample(50.0, 0, 1.0), sample(54.0, 10, 1.0)]);
        assert_eq!(dabs.len(), 11);
    }

    #[test]
    fn wash_dabs_ease_their_average_opacity_down() {
        let dabs = stroke(&airbrushing(), &[sample(50.0, 0, 1.0), sample(50.0, 3, 0.5)]);
        let first = dabs[0];
        assert!((first.opacity - 1.0).abs() < 1e-6);
        assert!((first.average_opacity - 1.0).abs() < 1e-6);
        // Lighter pressure lowers the dab opacity at once, its average slowly.
        let last = dabs[dabs.len() - 1];
        assert!(last.opacity < 0.7);
        assert!(last.average_opacity > 0.8);
        assert!(last.average_opacity < first.average_opacity);
    }

    #[test]
    fn without_wash_opacity_scales_flow() {
        let mut preset = BrushPreset::airbrush_soft(BrushPresetId(0));
        preset.paint_mode = PaintMode::Normal;
        let dabs = stroke(&preset, &[sample(50.0, 0, 0.5)]);
        let want = preset.dynamics.opacity.as_ref().map_or(1.0, |m| m.curve.sample(0.5));
        assert!((dabs[0].flow - want).abs() < 1e-6);
        assert!((dabs[0].opacity - 1.0).abs() < 1e-6);
    }
}
