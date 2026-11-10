//! GPU smoke tests for the built-in brushes' rendering paths (Krita-accurate
//! Airbrush Soft + the Chalk image-tip/texture path).

#![allow(clippy::unwrap_used)]
// Diagnostic prints for the --nocapture runs of these GPU tests.
#![allow(clippy::print_stdout)]

use oxiedraw_core::brush_engine::{BrushEngine, InputSample};
use oxiedraw_core::canvas::Canvas;
use oxiedraw_core::color::Color;
use oxiedraw_utils::geometry::{Point, Size};

fn sample_p(x: f32, y: f32, t: u64, pressure: f32) -> InputSample {
    InputSample {
        position: Point::new(x, y),
        pressure,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rotation: 0.0,
        time_ms: t,
    }
}

fn select(brush: &BrushEngine, name: &str) {
    let id = brush
        .brushes
        .borrow()
        .iter()
        .find(|p| p.name == name)
        .map_or_else(|| panic!("preset {name} exists"), |p| p.id);
    brush.active.set(id);
}

/// Paint a short horizontal stroke of the active (mask) brush and return the
/// active layer's BGRA8 pixels.
fn paint(name: &str, size: Size) -> (Vec<u8>, usize) {
    paint_pressure(name, size, 1.0)
}

/// Like `paint` but with a fixed pen pressure across the stroke.
fn paint_pressure(name: &str, size: Size, pressure: f32) -> (Vec<u8>, usize) {
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer("l").unwrap();
    canvas.layers().set_active(Some(idx));

    let brush = BrushEngine::new();
    select(&brush, name);
    brush.size.set(28.0);
    brush.opacity.set(1.0);

    let white = Color::new(255, 255, 255);
    canvas.begin_stroke(white, 1.0, false).unwrap();
    let preset = brush.active_brush();
    canvas
        .set_stroke_style(preset.paint_mode, preset.softness_curve.as_ref())
        .unwrap();
    let y = (size.height / 2) as f32;
    canvas
        .stamp(|t| brush.begin_stroke(sample_p(24.0, y, 0, pressure), white, t))
        .unwrap();
    for i in 1..=30 {
        let x = 24.0 + (size.width as f32 - 48.0) * (i as f32) / 30.0;
        canvas
            .stamp(|t| brush.push_sample(sample_p(x, y, i * 8, pressure), t))
            .unwrap();
    }
    canvas.stamp(|t| brush.end_stroke(t)).unwrap();
    canvas.commit_stroke().unwrap();

    let mut buf = Vec::new();
    canvas.read_layer_region_into(idx, 0, 0, size.width, size.height, &mut buf).unwrap();
    (buf, size.width as usize)
}

/// (solid, partial, total) alpha counts in the centre band of a stroke.
fn band_stats(buf: &[u8], size: Size, w: usize) -> (usize, usize, usize) {
    let band = 12usize;
    let cy = (size.height / 2) as usize;
    let (mut solid, mut partial, mut total) = (0usize, 0usize, 0usize);
    for y in (cy - band)..(cy + band) {
        for x in 40..(size.width as usize - 40) {
            let a = buf[(y * w + x) * 4 + 3];
            total += 1;
            if a > 200 {
                solid += 1;
            } else if a > 20 {
                partial += 1;
            }
        }
    }
    (solid, partial, total)
}

fn covered_pixels(buf: &[u8]) -> usize {
    buf.chunks_exact(4).filter(|px| px[3] > 20).count()
}

const SPOT: Size = Size::new(64, 64);
/// Centre of pixel (32, 32), so the dab's squared distance there is zero.
const CENTRE: f32 = 32.5;

/// Hold Airbrush Soft still on `SPOT` for `hold_ms` with its airbrush switched
/// on, painting or erasing the active layer; returns the layer's alpha.
fn airbrush_spot(pressure: f32, hold_ms: u64, erase: bool) -> Vec<u8> {
    let mut canvas = Canvas::headless(SPOT).unwrap();
    let idx = canvas.add_layer("l").unwrap();
    canvas.layers().set_active(Some(idx));
    if erase {
        canvas.clear([1.0, 1.0, 1.0, 1.0]).unwrap();
    }
    let brush = BrushEngine::new();
    select(&brush, "Airbrush Soft");
    for p in brush.brushes.borrow_mut().iter_mut() {
        p.airbrush = true;
        p.airbrush_rate = 1000.0;
    }
    brush.size.set(40.0);
    let preset = brush.active_brush();
    let white = Color::new(255, 255, 255);
    canvas.begin_stroke(white, 1.0, erase).unwrap();
    canvas
        .set_stroke_style(preset.paint_mode, preset.softness_curve.as_ref())
        .unwrap();
    canvas
        .stamp(|t| brush.begin_stroke(sample_p(CENTRE, CENTRE, 0, pressure), white, t))
        .unwrap();
    if hold_ms > 0 {
        canvas
            .stamp(|t| brush.push_sample(sample_p(CENTRE, CENTRE, hold_ms, pressure), t))
            .unwrap();
    }
    canvas.stamp(|t| brush.end_stroke(t)).unwrap();
    canvas.commit_stroke().unwrap();
    let mut buf = Vec::new();
    canvas
        .read_layer_region_into(idx, 0, 0, SPOT.width, SPOT.height, &mut buf)
        .unwrap();
    buf.chunks_exact(4).map(|px| px[3]).collect()
}

fn alpha_at(alpha: &[u8], x: usize, y: usize) -> u8 {
    alpha[y * SPOT.width as usize + x]
}

/// One dab, worked through Krita's 8-bit maths by hand: the soft curve starts
/// at 0.39911 -> 102, opacity(1.0) = 255, and alpha-darken from empty gives
/// lerp(0, 255, 102) = 102. At pressure 0.5 the opacity curve gives
/// 0.301251 -> 77 and lerp(0, 77, 102) = 31.
#[test]
fn airbrush_soft_dab_matches_krita_alpha() {
    assert_eq!(alpha_at(&airbrush_spot(1.0, 0, false), 32, 32), 102);
    assert_eq!(alpha_at(&airbrush_spot(0.5, 0, false), 32, 32), 31);
}

/// Krita's wash: held still, the airbrush keeps spraying, and the coverage
/// builds up to the pen's opacity and stops there. Krita's 8-bit lerp rounds
/// the last step away (lerp(76, 77, 102) == 76), so the centre settles one
/// below the ceiling.
#[test]
fn airbrush_soft_builds_up_to_the_pressure_ceiling() {
    let single = airbrush_spot(0.5, 0, false);
    let held = airbrush_spot(0.5, 600, false);
    assert_eq!(alpha_at(&held, 32, 32), 76, "centre settles under opacity(0.5)");
    assert!(held.iter().all(|&a| a <= 77), "wash went past its ceiling");
    let edge = (alpha_at(&single, 46, 32), alpha_at(&held, 46, 32));
    assert!(edge.1 > edge.0, "holding still didn't build up the edge: {edge:?}");
    assert_eq!(alpha_at(&airbrush_spot(1.0, 600, false), 32, 32), 254);
}

/// Like Krita, a moving stroke keeps the soft curve's falloff across its
/// width: dabs land only every 0.1 diameter, so the faint edge never piles up
/// to the opacity ceiling the way a 1000 dab/s airbrush made it.
#[test]
fn airbrush_soft_stroke_stays_soft_across_its_width() {
    let size = Size::new(200, 60);
    let (buf, w) = paint("Airbrush Soft", size);
    let alpha = |y: usize| buf[(y * w + 100) * 4 + 3];
    // Brush size 28: 11 px off the centre line is 0.8 of the radius.
    let (centre, edge) = (alpha(30), alpha(41));
    println!("airbrush soft cross-section: centre {centre}, 0.8R {edge}");
    assert!(centre > 150, "centre too faint: {centre}");
    assert!(u32::from(edge) * 3 < u32::from(centre), "edge too solid: {edge} vs {centre}");
}

/// The drag path stamps, previews and presents in one submit; its wash must
/// land the same pixels as the plain stamp path.
#[test]
fn airbrush_soft_drag_path_matches_plain_stamping() {
    let run = |present: bool| {
        let mut canvas = Canvas::headless(SPOT).unwrap();
        let idx = canvas.add_layer("l").unwrap();
        canvas.layers().set_active(Some(idx));
        let brush = BrushEngine::new();
        select(&brush, "Airbrush Soft");
        brush.size.set(40.0);
        let preset = brush.active_brush();
        let white = Color::new(255, 255, 255);
        canvas.begin_stroke(white, 1.0, false).unwrap();
        canvas
            .set_stroke_style(preset.paint_mode, preset.softness_curve.as_ref())
            .unwrap();
        canvas
            .stamp(|t| brush.begin_stroke(sample_p(20.0, 32.0, 0, 0.8), white, t))
            .unwrap();
        for i in 1..=12u64 {
            let s = sample_p(20.0 + 2.0 * i as f32, 32.0, i * 4, 0.8);
            if present {
                canvas.stamp_and_present(|t| brush.push_sample(s, t)).unwrap();
            } else {
                canvas.stamp(|t| brush.push_sample(s, t)).unwrap();
            }
        }
        canvas.stamp(|t| brush.end_stroke(t)).unwrap();
        canvas.commit_stroke().unwrap();
        let mut buf = Vec::new();
        canvas
            .read_layer_region_into(idx, 0, 0, SPOT.width, SPOT.height, &mut buf)
            .unwrap();
        buf
    };
    let plain = run(false);
    assert!(plain.chunks_exact(4).any(|px| px[3] > 100), "stroke painted nothing");
    assert_eq!(plain, run(true));
}

/// The eraser runs the same stroke and takes away exactly the alpha the brush
/// would have laid down.
#[test]
fn airbrush_soft_erases_what_it_paints() {
    for pressure in [0.4, 1.0] {
        let painted = airbrush_spot(pressure, 200, false);
        let erased = airbrush_spot(pressure, 200, true);
        for (p, e) in painted.iter().zip(&erased) {
            assert!(
                (i32::from(*p) + i32::from(*e) - 255).abs() <= 1,
                "painted {p} but erasing left {e} at pressure {pressure}"
            );
        }
    }
}

/// Chalk uses the image tip + SUBTRACT paper texture, so its stroke must have
/// broken/grainy coverage - both solid and holey pixels inside its footprint,
/// unlike a solid round brush.
#[test]
fn chalk_stroke_is_grainy() {
    let size = Size::new(220, 60);
    let (buf, w) = paint("Chalk", size);
    let covered = covered_pixels(&buf);
    assert!(covered > 500, "chalk deposited nothing meaningful: {covered}");

    // Inside the stroke band, count fully-covered vs empty pixels. A grainy
    // brush has a healthy mix of both (the texture punches holes); a plain
    // solid brush would be almost all covered.
    // Inside the stroke band, count fully-covered vs partial pixels. Krita's
    // Chalk_Soft is soft (gentle subtract), so it doesn't punch hard holes but
    // it's markedly grainy: unlike the near-solid Default Round, well under
    // half the band reads fully opaque, with lots of partial coverage.
    let band = 12usize; // +/- around centre row
    let cy = (size.height / 2) as usize;
    let (mut solid, mut partial, mut total) = (0usize, 0usize, 0usize);
    for y in (cy - band)..(cy + band) {
        for x in 40..(size.width as usize - 40) {
            let a = buf[(y * w + x) * 4 + 3];
            total += 1;
            if a > 200 {
                solid += 1;
            } else if a > 20 {
                partial += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let solid_frac = solid as f32 / total as f32;
    println!("chalk band: solid={solid} partial={partial} total={total} solid_frac={solid_frac:.2}");
    assert!(solid > 0, "chalk has no solid deposit");
    assert!(partial > solid, "chalk not grainy enough - too little partial coverage");
    assert!(solid_frac < 0.6, "chalk reads near-solid (grain/texture not applied): {solid_frac:.2}");
}

/// Charcoal Pencil is a Textured brush: a soft round tip modulated by the
/// dotted paper texture (MULTIPLY), with texture strength driven *down* by
/// pressure (Krita's `Texture/Strength` sensor). So a light touch reads
/// grainy/broken and a firm press lays a solid, dark line - the opposite ends
/// must look clearly different.
#[test]
fn charcoal_pencil_pressure_goes_grainy_to_solid() {
    let size = Size::new(220, 60);

    // Light touch: texture strength ~1 -> grainy, mostly partial coverage.
    let (light, w) = paint_pressure("Charcoal Pencil", size, 0.35);
    let (l_solid, l_partial, l_total) = band_stats(&light, size, w);
    #[allow(clippy::cast_precision_loss)]
    let light_solid_frac = l_solid as f32 / l_total as f32;
    println!(
        "charcoal light: solid={l_solid} partial={l_partial} total={l_total} frac={light_solid_frac:.2}"
    );
    assert!(l_partial > 0, "light charcoal has no textured coverage");
    assert!(
        light_solid_frac < 0.25,
        "light charcoal should be grainy, not near-solid: {light_solid_frac:.2}"
    );

    // Firm press: texture strength ~0 -> solid, most of the core reads opaque.
    let (firm, _w) = paint_pressure("Charcoal Pencil", size, 1.0);
    let (f_solid, f_partial, f_total) = band_stats(&firm, size, w);
    #[allow(clippy::cast_precision_loss)]
    let firm_solid_frac = f_solid as f32 / f_total as f32;
    println!(
        "charcoal firm: solid={f_solid} partial={f_partial} total={f_total} frac={firm_solid_frac:.2}"
    );
    assert!(
        firm_solid_frac > light_solid_frac + 0.2,
        "pressure did not solidify the stroke: light={light_solid_frac:.2} firm={firm_solid_frac:.2}"
    );
}
