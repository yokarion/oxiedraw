//! GPU smoke tests for the built-in brushes' rendering paths (Krita-accurate
//! Default Round + the Chalk image-tip/texture path). Run with
//! `cargo test -p oxiedraw-core --test brushes_gpu -- --ignored --nocapture`.

#![allow(clippy::unwrap_used)]
// Diagnostic prints for the --nocapture runs of these ignored GPU tests.
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
    canvas.set_stroke_buildup(brush.active_brush().buildup);
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

#[test]
#[ignore = "requires vulkan loader and device"]
fn default_round_deposits_a_solid_stroke() {
    let size = Size::new(200, 60);
    let (buf, _w) = paint("Default Round", size);
    let covered = covered_pixels(&buf);
    println!("Default Round covered pixels: {covered}");
    assert!(covered > 1500, "default round stroke too sparse: {covered}");
}

/// Chalk uses the image tip + SUBTRACT paper texture, so its stroke must have
/// broken/grainy coverage - both solid and holey pixels inside its footprint,
/// unlike the solid Default Round.
#[test]
#[ignore = "requires vulkan loader and device"]
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
#[ignore = "requires vulkan loader and device"]
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
