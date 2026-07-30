//! Render brush stroke previews through the real Vulkan engine.
//!
//! Each preview is a small headless `Canvas` painted with an S-curve
//! stroke whose pressure follows a `sin(pit)` envelope (0 -> 1 -> 0). The
//! result is read back as BGRA8, swapped to RGBA, and encoded as a PNG
//! so it can be cached inside the `.oxiebrush` archive.
//!
//! The stroke is painted in *white* on a transparent background - the
//! display path treats the alpha channel as a mask and recolours with
//! the theme foreground colour, so previews stay theme-aware despite
//! being cached.
//!
//! A single 320x80 canvas is held in a `thread_local!` `RefCell` so the
//! Vulkan instance + device are created exactly once. Subsequent
//! `render_preview_png` calls reuse it.
//!
//! This module is the *only* place inside `brush_engine` that talks to
//! the renderer. Other modules deliberately stay engine-only so they
//! can run in headless CI.
//!
//! Failure modes: if the Vulkan canvas can't be created (no GPU,
//! driver fault, sandbox restriction), `render_preview_png` returns
//! `Err`. Callers are expected to fall back to the legacy Cairo
//! approximation in that case.
//!
//! Re-entrancy: `thread_local!` guarantees one borrow at a time. Don't
//! invoke `render_preview_png` from inside a paint callback that
//! already holds the previewer - there's nothing useful to render mid-
//! stroke anyway.

use std::cell::RefCell;
use std::io::Cursor;

use oxiedraw_utils::geometry::{Point, Size};

use crate::canvas::Canvas;
use crate::color::Color;

use super::input::InputSample;
use super::preset::BrushPreset;
use super::brush::StrokeContext;
use super::stamp::start_stroke;
use super::brush::BrushPresetId;

/// Preview canvas dimensions. Picks a value that's roomy enough for
/// the editor's large preview (~640x160 displayed via 2x scaling) and
/// keeps PNG size to a few dozen KB.
const PREVIEW_WIDTH: u32 = 320;
const PREVIEW_HEIGHT: u32 = 80;

/// How many input samples we feed the engine. More samples = smoother
/// Catmull-Rom spline, but also more dabs. 24 covers the full S-curve
/// without over-stamping.
const SAMPLE_COUNT: usize = 24;

/// Maximum stroke radius as a fraction of canvas height. Big brushes
/// get scaled down so they fit; small brushes are not magnified.
const MAX_RADIUS_FRACTION: f32 = 0.42;

thread_local! {
    /// Lazily-initialised preview canvas. We don't try to recover from
    /// init failure inside this cell - a failed init is recorded as
    /// `None` so subsequent calls fast-fail without re-attempting
    /// Vulkan instance creation.
    static PREVIEW_CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
}

/// Render `preset` to a PNG-encoded preview suitable for caching in
/// the `.oxiebrush` archive. The image is white-on-transparent so the
/// alpha channel can be used as a recolour mask at display time.
///
/// Returns `Err(reason)` if the canvas can't be initialised or if the
/// stroke fails. Callers should fall back to a Cairo preview in that
/// case rather than treating it as fatal.
pub fn render_preview_png(preset: &BrushPreset) -> Result<Vec<u8>, String> {
    PREVIEW_CANVAS.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let canvas = Canvas::headless(Size::new(PREVIEW_WIDTH, PREVIEW_HEIGHT))
                .map_err(|e| format!("preview canvas init failed: {e}"))?;
            *slot = Some(canvas);
        }
        let canvas = slot.as_mut().expect("preview canvas initialised above");
        render_into(canvas, preset).map_err(|e| format!("preview render failed: {e}"))?;
        let bgra = canvas
            .read_pixels()
            .map_err(|e| format!("preview readback failed: {e}"))?;
        encode_bgra_to_png(&bgra, PREVIEW_WIDTH, PREVIEW_HEIGHT)
    })
}

/// Drive a stroke through the engine onto the preview canvas. Clears
/// the active layer to transparent first so previous previews don't
/// bleed through.
fn render_into(canvas: &mut Canvas, preset: &BrushPreset) -> Result<(), crate::renderer::RendererError> {
    // The atlas dedup is keyed on `Rc::as_ptr`. Every brush reload
    // mints a fresh `Rc<PatternData>` for the same content, so without
    // this reset we'd burn one of 16 slices every save and lock up
    // after a dozen-ish edits of a Textured brush. The preview canvas
    // is dedicated - nothing else holds slice references between
    // renders - so clearing here is safe.
    canvas.clear_pattern_atlas();
    canvas.clear([0.0, 0.0, 0.0, 0.0])?;

    let base_size = scale_to_fit(preset.default_size);
    let ctx = StrokeContext {
        preset: BrushPresetId(0),
        color: Color::WHITE,
        size: base_size,
        opacity: 1.0,
    };

    canvas.begin_stroke(ctx.color, ctx.opacity, false)?;
    // Smudge brushes paint through the dedicated GPU path, not the mask
    // pipelines, so route the preview stroke there too (otherwise it would
    // render as a plain soft-round mask via the family resolver).
    if preset.family.is_smudge() {
        canvas.set_smudge_stroke(true)?;
    }
    canvas.stamp(|target| {
        let mut renderer = start_stroke(preset, ctx);
        let samples = build_samples(base_size);
        for sample in samples {
            renderer.push(sample, target);
        }
        renderer.end(target);
    })?;
    canvas.commit_stroke()?;
    Ok(())
}

/// Clamp the brush's base diameter so the rendered stroke fits inside
/// the preview canvas without clipping. Small brushes pass through
/// unchanged (we don't magnify - pixel brushes especially shouldn't
/// be blown up to a smear).
fn scale_to_fit(default_size: f32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let max_diameter = PREVIEW_HEIGHT as f32 * MAX_RADIUS_FRACTION * 2.0;
    default_size.min(max_diameter).max(1.0)
}

/// Synthesise the S-curve sample stream. `base_size` is the
/// already-scaled brush diameter - sample timing assumes a constant
/// 8 ms tick so the `Speed` dynamic source sees something plausible.
fn build_samples(_base_size: f32) -> Vec<InputSample> {
    #[allow(clippy::cast_precision_loss)]
    let canvas_w = PREVIEW_WIDTH as f32;
    #[allow(clippy::cast_precision_loss)]
    let canvas_h = PREVIEW_HEIGHT as f32;
    let pad_x = 30.0_f32;
    let mid_y = canvas_h * 0.5;
    let amp = canvas_h * 0.28;
    let usable_w = pad_x.mul_add(-2.0, canvas_w);

    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for i in 0..SAMPLE_COUNT {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f32 / (SAMPLE_COUNT - 1) as f32;
        let x = pad_x + usable_w * t;
        // Half-sine arc - matches the cairo preview shape.
        let phase = (t - 0.5) * std::f32::consts::PI * 1.6;
        let y = phase.sin().mul_add(amp, mid_y);
        let pressure = (t * std::f32::consts::PI).sin();
        samples.push(InputSample {
            position: Point::new(x, y),
            pressure,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation: 0.0,
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            time_ms: (i as u64) * 8,
        });
    }
    samples
}

/// BGRA8 -> PNG via the `png` crate. The renderer's premultiplied
/// canvas readback is white-tinted by alpha; we undo premultiplication
/// so the cached PNG stores conventional straight-alpha pixels.
fn encode_bgra_to_png(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let pixel_count = (width as usize) * (height as usize);
    if bgra.len() != pixel_count * 4 {
        return Err(format!(
            "bgra length {} doesn't match {}x{}x4",
            bgra.len(),
            width,
            height
        ));
    }
    let mut rgba_straight = Vec::with_capacity(bgra.len());
    for chunk in bgra.chunks_exact(4) {
        let b = chunk[0];
        let g = chunk[1];
        let r = chunk[2];
        let a = chunk[3];
        if a == 0 {
            rgba_straight.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let inv = 255.0 / f32::from(a);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                rgba_straight.push((f32::from(r) * inv).min(255.0) as u8);
                rgba_straight.push((f32::from(g) * inv).min(255.0) as u8);
                rgba_straight.push((f32::from(b) * inv).min(255.0) as u8);
                rgba_straight.push(a);
            }
        }
    }
    let mut out: Vec<u8> = Vec::new();
    {
        let cursor = Cursor::new(&mut out);
        let mut encoder = png::Encoder::new(cursor, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(&rgba_straight)
            .map_err(|e| e.to_string())?;
    }
    Ok(out)
}
