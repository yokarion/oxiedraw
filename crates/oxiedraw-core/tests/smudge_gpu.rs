//! GPU integration tests for the colour-smudge (Real Brush) path. Run with
//! `cargo test -p oxiedraw-core --test smudge_gpu -- --ignored --nocapture`.

#![allow(clippy::unwrap_used)]

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


fn select_real_brush(brush: &BrushEngine) {
    let id = brush
        .brushes
        .borrow()
        .iter()
        .find(|p| p.name == "Real Brush")
        .map(|p| p.id)
        .expect("Real Brush preset exists");
    brush.active.set(id);
}

/// Drive a straight horizontal smudge stroke across `y`, mirroring the UI:
/// begin_stroke + set_smudge_stroke(true) + a series of stamped samples.
fn smudge_line(canvas: &mut Canvas, brush: &BrushEngine, color: Color, y: f32, x0: f32, x1: f32) {
    smudge_line_p(canvas, brush, color, y, x0, x1, 1.0);
}

fn smudge_line_p(
    canvas: &mut Canvas,
    brush: &BrushEngine,
    color: Color,
    y: f32,
    x0: f32,
    x1: f32,
    pressure: f32,
) {
    canvas.begin_stroke(color, brush.opacity.get(), false).unwrap();
    canvas.set_smudge_stroke(true).unwrap();
    let steps = 40;
    let mut t = 0u64;
    canvas
        .stamp(|tg| brush.begin_stroke(sample_p(x0, y, t, pressure), color, tg))
        .unwrap();
    // Drive the mid-stroke samples through the interactive hot path
    // (`stamp_and_present`, the combined async submit), like the real UI.
    for i in 1..=steps {
        t += 8;
        let x = x0 + (x1 - x0) * (i as f32) / (steps as f32);
        canvas
            .stamp_and_present(|tg| brush.push_sample(sample_p(x, y, t, pressure), tg))
            .unwrap();
    }
    canvas.stamp(|tg| brush.end_stroke(tg)).unwrap();
    canvas.commit_stroke().unwrap();
}

/// White smudge over transparent must stay light - premultiplied-black pickup
/// used to smear dark. Reads the active layer's alpha + luma along the stroke.
#[test]
#[ignore = "requires vulkan loader and device"]
fn white_smudge_over_transparent_is_not_dark() {
    let size = Size::new(200, 40);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer("smudge").unwrap();
    canvas.layers().set_active(Some(idx));

    let brush = BrushEngine::new();
    select_real_brush(&brush);
    brush.size.set(20.0);
    brush.opacity.set(1.0);

    smudge_line(&mut canvas, &brush, Color::new(255, 255, 255), 20.0, 30.0, 170.0);

    let mut buf = Vec::new();
    canvas.read_layer_region_into(idx, 0, 0, size.width, size.height, &mut buf).unwrap();
    // BGRA8 premultiplied. Scan the whole stroke band for the "dark blob"
    // signature: a covered pixel whose un-premultiplied colour reads dark.
    let w = size.width as usize;
    let mut max_alpha = 0u8;
    let mut dark_count = 0;
    for y in 0..size.height as usize {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let (b, g, r, a) = (buf[i], buf[i + 1], buf[i + 2], buf[i + 3]);
            max_alpha = max_alpha.max(a);
            if a > 60 {
                let luma = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
                let straight = (luma * 255 / u32::from(a)).min(255);
                if straight < 180 {
                    dark_count += 1;
                }
            }
        }
    }
    println!("max_alpha={max_alpha} dark_pixels={dark_count}");
    assert!(max_alpha > 60, "stroke deposited some coverage");
    assert_eq!(dark_count, 0, "white smudge over transparent must not read dark");
}

/// Smudge must actually smear: painting from a red region into a blue region
/// with no paint colour (colour rate ~0 via zero pressure keeps it a pure
/// smear) drags red into blue at the boundary.
#[test]
#[ignore = "requires vulkan loader and device"]
fn smudge_drags_colour_across_a_boundary() {
    let size = Size::new(200, 40);
    let mut canvas = Canvas::headless(size).unwrap();
    // Left half red, right half blue (opaque).
    let mut px = vec![0u8; (size.width * size.height) as usize * 4];
    let w = size.width as usize;
    for y in 0..size.height as usize {
        for x in 0..w {
            let i = (y * w + x) * 4;
            if x < w / 2 {
                px[i..i + 4].copy_from_slice(&[0, 0, 255, 255]); // red
            } else {
                px[i..i + 4].copy_from_slice(&[255, 0, 0, 255]); // blue
            }
        }
    }
    let idx = canvas.add_layer_with_pixels("base", &px).unwrap();
    canvas.layers().set_active(Some(idx));

    let brush = BrushEngine::new();
    select_real_brush(&brush);
    brush.size.set(20.0);
    brush.opacity.set(1.0);
    // Smear from red into blue.
    smudge_line(&mut canvas, &brush, Color::new(0, 0, 0), 20.0, 40.0, 160.0);

    let mut buf = Vec::new();
    canvas.read_layer_region_into(idx, 0, 0, size.width, size.height, &mut buf).unwrap();
    // Just past the boundary into the blue side, red should have bled in
    // (red channel raised above the pure-blue baseline of 0).
    let row = 20usize;
    let sample_x = w / 2 + 6;
    let i = (row * w + sample_x) * 4;
    let red = buf[i + 2];
    println!("red bled into blue side at x={sample_x}: r={red}");
    assert!(red > 20, "smudge did not drag red across the boundary (r={red})");
}

/// White smudge over an OPAQUE BLACK layer must not bead into periodic
/// lighter dots along the stroke (the user's report). Measure the luma
/// variance along the centre line.
#[test]
#[ignore = "requires vulkan loader and device"]
fn white_smudge_on_black_is_not_dotty() {
    let size = Size::new(300, 40);
    let mut canvas = Canvas::headless(size).unwrap();
    let black = vec_black(size);
    let idx = canvas.add_layer_with_pixels("base", &black).unwrap();
    canvas.layers().set_active(Some(idx));

    let brush = BrushEngine::new();
    select_real_brush(&brush);
    brush.size.set(22.0);
    brush.opacity.set(1.0);
    smudge_line(&mut canvas, &brush, Color::new(255, 255, 255), 20.0, 30.0, 270.0);

    let mut buf = Vec::new();
    canvas.read_layer_region_into(idx, 0, 0, size.width, size.height, &mut buf).unwrap();
    let row = 20usize;
    let w = size.width as usize;
    // Layer is opaque, so read straight luma along the centre of the stroke.
    let lumas: Vec<i32> = (60..240)
        .map(|x| {
            let i = (row * w + x) * 4;
            (i32::from(buf[i]) + i32::from(buf[i + 1]) + i32::from(buf[i + 2])) / 3
        })
        .collect();
    let min = *lumas.iter().min().unwrap();
    let max = *lumas.iter().max().unwrap();
    let mean = lumas.iter().sum::<i32>() / lumas.len() as i32;
    println!("black-bg centre luma: min={min} max={max} mean={mean} spread={}", max - min);
    // A dotty stroke has big periodic luma swings; a smooth one is tight.
    assert!(max - min < 60, "smudge on black is dotty: luma spread {}", max - min);
}

/// The opacity slider must scale the smudge deposit: a low-opacity white
/// smudge on black stays much darker than a full-opacity one.
#[test]
#[ignore = "requires vulkan loader and device"]
fn opacity_scales_smudge_deposit() {
    let mean_luma_at = |opacity: f32| -> i32 {
        let size = Size::new(240, 40);
        let mut canvas = Canvas::headless(size).unwrap();
        let idx = canvas.add_layer_with_pixels("base", &vec_black(size)).unwrap();
        canvas.layers().set_active(Some(idx));
        let brush = BrushEngine::new();
        select_real_brush(&brush);
        brush.size.set(22.0);
        brush.opacity.set(opacity);
        smudge_line(&mut canvas, &brush, Color::new(255, 255, 255), 20.0, 30.0, 210.0);
        let mut buf = Vec::new();
        canvas.read_layer_region_into(idx, 0, 0, size.width, size.height, &mut buf).unwrap();
        let w = size.width as usize;
        let lumas: Vec<i32> = (60..180)
            .map(|x| {
                let i = (20 * w + x) * 4;
                (i32::from(buf[i]) + i32::from(buf[i + 1]) + i32::from(buf[i + 2])) / 3
            })
            .collect();
        lumas.iter().sum::<i32>() / lumas.len() as i32
    };
    let low = mean_luma_at(0.25);
    let high = mean_luma_at(1.0);
    println!("mean luma: opacity0.25={low} opacity1.0={high}");
    assert!(high - low > 40, "opacity slider had little effect (low={low} high={high})");
}

/// The pre-stroke smudge snapshot (used for undo) must hold the PRISTINE layer
/// content, even though the layer itself is mutated live during the stroke.
#[test]
#[ignore = "requires vulkan loader and device"]
fn smudge_before_snapshot_is_pristine() {
    let size = Size::new(120, 40);
    let mut canvas = Canvas::headless(size).unwrap();
    // Opaque red base.
    let mut red = vec![0u8; (size.width * size.height) as usize * 4];
    for p in red.chunks_exact_mut(4) {
        p.copy_from_slice(&[0, 0, 255, 255]);
    }
    let idx = canvas.add_layer_with_pixels("base", &red).unwrap();
    canvas.layers().set_active(Some(idx));

    let brush = BrushEngine::new();
    select_real_brush(&brush);
    brush.size.set(20.0);
    brush.opacity.set(1.0);
    // Paint a white smudge stroke over the red (mutates the layer live).
    smudge_line(&mut canvas, &brush, Color::new(255, 255, 255), 20.0, 20.0, 100.0);

    // The layer now differs from red under the stroke...
    let mut after = Vec::new();
    canvas.read_layer_region_into(idx, 40, 18, 8, 4, &mut after).unwrap();
    // ...but the before-snapshot must still read the original red there.
    let mut before = Vec::new();
    canvas.read_smudge_before_region_into(40, 18, 8, 4, &mut before).unwrap();
    assert_eq!(before.len(), 8 * 4 * 4, "before region has the requested size");
    for px in before.chunks_exact(4) {
        // BGRA red, opaque.
        assert!(px[2] > 240 && px[0] < 15 && px[1] < 15 && px[3] > 240,
            "before-snapshot not pristine red: {px:?}");
    }
    assert_ne!(before, after, "layer should have changed under the stroke");
}

fn vec_black(size: Size) -> Vec<u8> {
    let mut px = vec![0u8; (size.width * size.height) as usize * 4];
    for p in px.chunks_exact_mut(4) {
        p.copy_from_slice(&[0, 0, 0, 255]);
    }
    px
}

/// Smudge over a solid colour should smear it, and the deposited alpha along
/// the stroke centre should be reasonably even (no strong per-dab beading).
#[test]
#[ignore = "requires vulkan loader and device"]
fn smudge_stroke_alpha_is_even() {
    let size = Size::new(240, 40);
    let mut canvas = Canvas::headless(size).unwrap();
    // Opaque red base so there's colour to smear.
    let mut red = vec![0u8; (size.width * size.height) as usize * 4];
    for px in red.chunks_exact_mut(4) {
        px.copy_from_slice(&[0, 0, 255, 255]); // BGRA red
    }
    let idx = canvas.add_layer_with_pixels("base", &red).unwrap();
    canvas.layers().set_active(Some(idx));

    let brush = BrushEngine::new();
    select_real_brush(&brush);
    brush.size.set(20.0);
    brush.opacity.set(1.0);

    smudge_line(&mut canvas, &brush, Color::new(0, 0, 255), 20.0, 30.0, 210.0);

    let mut buf = Vec::new();
    canvas.read_layer_region_into(idx, 0, 0, size.width, size.height, &mut buf).unwrap();
    let row = 20usize;
    let w = size.width as usize;
    let alphas: Vec<u8> = (40..200).map(|x| buf[(row * w + x) * 4 + 3]).collect();
    let min = *alphas.iter().min().unwrap();
    let max = *alphas.iter().max().unwrap();
    println!("centre-row alpha over stroke: min={min} max={max}");
    // Base is opaque; smear keeps it opaque - so alpha should stay near 255
    // everywhere, i.e. no holes/beading punched into the coverage.
    assert!(min > 220, "smudge punched low-alpha beads into an opaque layer (min={min})");
}

/// Low-pressure white smudge over transparent: the classic OVER-of-soft-dabs
/// beading case. Report the centre-row alpha variance so we can see beads.
#[test]
#[ignore = "requires vulkan loader and device"]
fn low_pressure_smudge_beading_profile() {
    let size = Size::new(240, 40);
    let mut canvas = Canvas::headless(size).unwrap();
    let idx = canvas.add_layer("smudge").unwrap();
    canvas.layers().set_active(Some(idx));

    let brush = BrushEngine::new();
    select_real_brush(&brush);
    brush.size.set(24.0);
    brush.opacity.set(1.0);

    smudge_line_p(&mut canvas, &brush, Color::new(255, 255, 255), 20.0, 30.0, 210.0, 0.45);

    let mut buf = Vec::new();
    canvas.read_layer_region_into(idx, 0, 0, size.width, size.height, &mut buf).unwrap();
    let row = 20usize;
    let w = size.width as usize;
    // Only sample the well-inside part of the stroke (skip the round ends).
    let alphas: Vec<u8> = (60..180).map(|x| buf[(row * w + x) * 4 + 3]).collect();
    let min = *alphas.iter().min().unwrap();
    let max = *alphas.iter().max().unwrap();
    let mean = alphas.iter().map(|&a| u32::from(a)).sum::<u32>() / alphas.len() as u32;
    println!("low-pressure centre alpha: min={min} max={max} mean={mean} spread={}", max - min);
    // Beading shows up as a large min/max spread along a nominally uniform line.
    assert!(u32::from(max - min) < 40, "beading: alpha spread {} too large", max - min);
}
