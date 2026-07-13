//! Rasterize [`TextContent`] into a canvas-sized layer slot.
//!
//! Shapes the runs with cosmic-text (per-run font, size, weight, style and
//! colour via [`cosmic_text::Attrs`]), rasterizes the glyphs to premultiplied
//! sRGB BGRA8, draws underlines, and places the block in the layer slot at the
//! text box position/alignment. Axis-aligned boxes blit directly (crisp);
//! rotated boxes go through the same affine resample as component instances.

use cosmic_text::{Align, Attrs, Buffer, Color as CtColor, Family, Metrics, Shaping, Style, Weight, Wrap};
use oxiedraw_utils::geometry::TransformFilter;
use oxiedraw_utils::pixels::transform_bgra8;

use super::fonts::TextEngine;
use super::{HAlign, ResizeMode, TextContent, TextStyle, VAlign};
use crate::color::Color;

/// Line height as a multiple of font size when a run doesn't specify one.
const LINE_HEIGHT_FACTOR: f32 = 1.2;

/// Angles below this (radians) are treated as axis-aligned (direct blit).
const ANGLE_EPSILON: f32 = 1e-3;

/// Upper bound on the local render buffer for rotated boxes, to cap memory.
const MAX_LOCAL_DIM: u32 = 8192;

/// Natural size (width, height) in pixels of the shaped content, given an
/// optional wrap width. `wrap_width = None` means no wrapping (auto width).
#[must_use]
pub fn measure(content: &TextContent, engine: &mut TextEngine, wrap_width: Option<f32>) -> (f32, f32) {
    let buffer = shape(content, engine, wrap_width);
    block_size(&buffer)
}

/// Render `content` into a fresh canvas-sized premultiplied BGRA8 buffer.
#[must_use]
pub fn render_text(
    content: &TextContent,
    engine: &mut TextEngine,
    canvas_w: u32,
    canvas_h: u32,
) -> Vec<u8> {
    let out_len = (canvas_w as usize) * (canvas_h as usize) * 4;
    if content.is_empty() {
        return vec![0u8; out_len];
    }
    let buffer = shape(content, engine, wrap_width_for(content));
    paint_buffer_scaled(
        &buffer,
        content.box_rect,
        content.scale,
        content.resize,
        content.v_align,
        content.default_style.color,
        engine,
        canvas_w,
        canvas_h,
    )
}

/// Rasterize a single line of `text` in `family` at `px` in `color` into a
/// tight premultiplied BGRA8 image, returning `(pixels, width, height)`.
///
/// Used to pre-render font-name previews so a long font list scrolls without
/// loading fonts live.
#[must_use]
pub fn render_label(
    engine: &mut TextEngine,
    text: &str,
    family: &str,
    px: f32,
    color: Color,
) -> (Vec<u8>, u32, u32) {
    let px = px.max(1.0);
    let metrics = Metrics::new(px, px * LINE_HEIGHT_FACTOR);
    let mut buffer = Buffer::new(&mut engine.font_system, metrics);
    buffer.set_wrap(&mut engine.font_system, Wrap::None);
    buffer.set_size(&mut engine.font_system, None, None);
    let attrs = Attrs::new()
        .family(Family::Name(family))
        .color(ct_color(color));
    buffer.set_text(&mut engine.font_system, text, attrs, Shaping::Advanced);
    buffer.shape_until_scroll(&mut engine.font_system, false);

    let (w, h) = block_size(&buffer);
    let w = (w.ceil() as u32).max(1);
    let h = (h.ceil() as u32).max(1);
    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    paint_into(
        &buffer,
        engine,
        &mut out,
        w as i32,
        h as i32,
        0,
        0,
        (0, 0, w as i32, h as i32),
        color,
    );
    (out, w, h)
}

/// Render `content` into a tight box-sized buffer (no rotation/translation):
/// the text laid out in its `box_rect.w x box_rect.h` local frame. Used as the
/// "master" texture for the Transform tool's live preview, so rotation/scale
/// are applied on top by the affine remap rather than baked into the pixels.
#[must_use]
pub fn render_text_local(content: &TextContent, engine: &mut TextEngine) -> (Vec<u8>, u32, u32) {
    let w = (content.box_rect.w.ceil() as u32).max(1);
    let h = (content.box_rect.h.ceil() as u32).max(1);
    if content.is_empty() {
        return (vec![0u8; (w as usize) * (h as usize) * 4], w, h);
    }
    let buffer = shape(content, engine, wrap_width_for(content));
    // Paint into a w x h slot with the box centred and unrotated.
    let local_box = super::TextBox::new(w as f32 / 2.0, h as f32 / 2.0, w as f32, h as f32, 0.0);
    let pixels = paint_buffer(
        &buffer,
        local_box,
        content.resize,
        content.v_align,
        content.default_style.color,
        engine,
        w,
        h,
    );
    (pixels, w, h)
}

/// Render the text at total scale `(sx, sy)` (over its natural box) into a tight
/// unrotated BGRA8 buffer `(pixels, w, h)`. Bakes `max(sx, sy)` into the glyph
/// size so the raster matches the target resolution; the residual squish is
/// downscaled. Used as the live Transform source for a crisp scaling drag.
#[must_use]
pub fn render_visible_local(
    content: &TextContent,
    sx: f32,
    sy: f32,
    engine: &mut TextEngine,
) -> (Vec<u8>, u32, u32) {
    let n = content.box_rect;
    let vw = ((n.w * sx).ceil() as u32).clamp(1, MAX_LOCAL_DIM);
    let vh = ((n.h * sy).ceil() as u32).clamp(1, MAX_LOCAL_DIM);
    if content.is_empty() {
        return (vec![0u8; (vw as usize) * (vh as usize) * 4], vw, vh);
    }
    let uniform = sx.max(sy).max(1e-3);
    let residual = (sx / uniform, sy / uniform);
    // Bake the uniform factor into the glyphs and lay out in a high-res box; the
    // residual squish then downscales that into the vw x vh buffer.
    let mut c = content.clone();
    for run in &mut c.runs {
        run.style.size = (run.style.size * uniform).max(1.0);
    }
    c.default_style.size = (c.default_style.size * uniform).max(1.0);
    #[allow(clippy::cast_precision_loss)]
    let hi_box = super::TextBox::new(vw as f32 / 2.0, vh as f32 / 2.0, n.w * uniform, n.h * uniform, 0.0);
    c.box_rect = hi_box;
    let buffer = shape(&c, engine, wrap_width_for(&c));
    let pixels = paint_buffer_scaled(
        &buffer, hi_box, residual, c.resize, c.v_align, c.default_style.color, engine, vw, vh,
    );
    (pixels, vw, vh)
}

/// Wrap width for a content's resize mode: `None` (no wrapping) for AutoWidth,
/// else the box width.
#[must_use]
pub(crate) fn wrap_width_for(content: &TextContent) -> Option<f32> {
    match content.resize {
        ResizeMode::AutoWidth => None,
        ResizeMode::AutoHeight | ResizeMode::Fixed => Some(content.box_rect.w.max(1.0)),
    }
}

/// Paint an already-shaped buffer's glyphs into a fresh canvas-sized slot at
/// the box position, honoring vertical alignment, Fixed-mode clipping, and box
/// rotation. Used by [`render_text`] and the live editor.
#[must_use]
pub(crate) fn paint_buffer(
    buffer: &Buffer,
    box_rect: super::TextBox,
    resize: ResizeMode,
    v_align: VAlign,
    default_color: Color,
    engine: &mut TextEngine,
    canvas_w: u32,
    canvas_h: u32,
) -> Vec<u8> {
    let out_len = (canvas_w as usize) * (canvas_h as usize) * 4;
    let (_block_w, block_h) = block_size(buffer);
    let top_offset = vertical_offset(v_align, box_rect.h, block_h);

    if box_rect.angle.abs() < ANGLE_EPSILON {
        // Axis-aligned: draw glyphs straight into the canvas slot (crisp).
        let mut out = vec![0u8; out_len];
        let origin_x = (box_rect.cx - box_rect.w / 2.0).round() as i32;
        let origin_y = (box_rect.cy - box_rect.h / 2.0 + top_offset).round() as i32;
        let clip = clip_bounds(resize, box_rect, canvas_w, canvas_h);
        paint_into(
            buffer,
            engine,
            &mut out,
            canvas_w as i32,
            canvas_h as i32,
            origin_x,
            origin_y,
            clip,
            default_color,
        );
        return out;
    }

    // Rotated: render into a local box-sized buffer, then affine-resample it
    // into the canvas slot (same path as component instances).
    let local_w = (box_rect.w.ceil() as u32).clamp(1, MAX_LOCAL_DIM);
    let local_h = (box_rect.h.ceil() as u32).clamp(1, MAX_LOCAL_DIM);
    let mut local = vec![0u8; (local_w as usize) * (local_h as usize) * 4];
    let local_clip = (0i32, 0i32, local_w as i32, local_h as i32);
    paint_into(
        buffer,
        engine,
        &mut local,
        local_w as i32,
        local_h as i32,
        0,
        top_offset.round() as i32,
        local_clip,
        default_color,
    );

    let original_rect = oxiedraw_utils::geometry::TransformRect::new(
        local_w as f32 / 2.0,
        local_h as f32 / 2.0,
        local_w as f32,
        local_h as f32,
        0.0,
    );
    transform_bgra8(
        &local,
        local_w,
        local_h,
        canvas_w,
        canvas_h,
        original_rect,
        box_rect.to_rect(),
        TransformFilter::Bilinear,
    )
}

/// Paint a shaped `buffer` into a canvas slot, applying an anamorphic display
/// `scale` (`(1.0, 1.0)` = straight paint). When scaled, the glyphs are laid
/// out at natural size in a local buffer then affine-remapped onto the visible
/// (scaled) rect - matching the Transform tool's live preview, and keeping the
/// text crisp/editable at its natural size. `box_rect` is the natural box.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_buffer_scaled(
    buffer: &Buffer,
    box_rect: super::TextBox,
    scale: (f32, f32),
    resize: ResizeMode,
    v_align: VAlign,
    default_color: Color,
    engine: &mut TextEngine,
    canvas_w: u32,
    canvas_h: u32,
) -> Vec<u8> {
    let scaled = (scale.0 - 1.0).abs() > 1e-3 || (scale.1 - 1.0).abs() > 1e-3;
    if !scaled {
        return paint_buffer(
            buffer, box_rect, resize, v_align, default_color, engine, canvas_w, canvas_h,
        );
    }
    // Render the natural layout into a tight, centred, unrotated local buffer.
    let lw = (box_rect.w.ceil() as u32).clamp(1, MAX_LOCAL_DIM);
    let lh = (box_rect.h.ceil() as u32).clamp(1, MAX_LOCAL_DIM);
    #[allow(clippy::cast_precision_loss)]
    let local_box = super::TextBox::new(lw as f32 / 2.0, lh as f32 / 2.0, lw as f32, lh as f32, 0.0);
    let local = paint_buffer(buffer, local_box, resize, v_align, default_color, engine, lw, lh);

    #[allow(clippy::cast_precision_loss)]
    let original_rect =
        oxiedraw_utils::geometry::TransformRect::new(lw as f32 / 2.0, lh as f32 / 2.0, lw as f32, lh as f32, 0.0);
    let target = oxiedraw_utils::geometry::TransformRect::new(
        box_rect.cx,
        box_rect.cy,
        box_rect.w * scale.0,
        box_rect.h * scale.1,
        box_rect.angle,
    );
    transform_bgra8(
        &local,
        lw,
        lh,
        canvas_w,
        canvas_h,
        original_rect,
        target,
        TransformFilter::Bilinear,
    )
}

/// Vertical offset (px) of the text block within a box of height `box_h`.
#[must_use]
pub(crate) fn vertical_offset(v_align: VAlign, box_h: f32, block_h: f32) -> f32 {
    match v_align {
        VAlign::Top => 0.0,
        VAlign::Middle => (box_h - block_h) / 2.0,
        VAlign::Bottom => box_h - block_h,
    }
}

/// Build a shaped cosmic-text buffer from the content's runs.
pub(crate) fn shape(content: &TextContent, engine: &mut TextEngine, wrap_width: Option<f32>) -> Buffer {
    let base_size = content.default_style.size.max(1.0);
    let metrics = Metrics::new(base_size, base_size * LINE_HEIGHT_FACTOR);
    let mut buffer = Buffer::new(&mut engine.font_system, metrics);

    let wrap = if wrap_width.is_some() {
        Wrap::WordOrGlyph
    } else {
        Wrap::None
    };
    buffer.set_wrap(&mut engine.font_system, wrap);
    buffer.set_size(&mut engine.font_system, wrap_width, None);

    let default_attrs = attrs_for(&content.default_style);
    let spans = content
        .runs
        .iter()
        .map(|run| (run.text.as_str(), attrs_for(&run.style)));
    buffer.set_rich_text(&mut engine.font_system, spans, default_attrs, Shaping::Advanced);

    // Per-line horizontal alignment within the wrap width.
    let align = match content.h_align {
        HAlign::Left => Align::Left,
        HAlign::Center => Align::Center,
        HAlign::Right => Align::Right,
    };
    for line in &mut buffer.lines {
        line.set_align(Some(align));
    }
    buffer.shape_until_scroll(&mut engine.font_system, false);
    buffer
}

/// cosmic-text attributes for one of our styles. Underline is carried in the
/// glyph metadata bit so the rasterizer can find underlined glyphs.
pub(crate) fn attrs_for(style: &TextStyle) -> Attrs<'_> {
    let size = style.size.max(1.0);
    Attrs::new()
        .family(Family::Name(style.font.as_str()))
        .weight(if style.bold { Weight::BOLD } else { Weight::NORMAL })
        .style(if style.italic { Style::Italic } else { Style::Normal })
        .color(ct_color(style.color))
        .metrics(Metrics::new(size, size * LINE_HEIGHT_FACTOR))
        .metadata(usize::from(style.underline))
}

/// Natural (width, height) of the laid-out text.
pub(crate) fn block_size(buffer: &Buffer) -> (f32, f32) {
    let mut w = 0.0f32;
    let mut h = 0.0f32;
    for run in buffer.layout_runs() {
        w = w.max(run.line_w);
        h = h.max(run.line_top + run.line_height);
    }
    (w, h)
}

/// Clip rectangle (x0, y0, x1, y1) in canvas pixels. Fixed boxes clip to the
/// box; auto modes only clip to the canvas (content already fits the box).
fn clip_bounds(resize: ResizeMode, box_rect: super::TextBox, canvas_w: u32, canvas_h: u32) -> (i32, i32, i32, i32) {
    let canvas = (0, 0, canvas_w as i32, canvas_h as i32);
    if resize != ResizeMode::Fixed {
        return canvas;
    }
    let bx0 = (box_rect.cx - box_rect.w / 2.0).floor() as i32;
    let by0 = (box_rect.cy - box_rect.h / 2.0).floor() as i32;
    let bx1 = (box_rect.cx + box_rect.w / 2.0).ceil() as i32;
    let by1 = (box_rect.cy + box_rect.h / 2.0).ceil() as i32;
    (
        bx0.max(canvas.0),
        by0.max(canvas.1),
        bx1.min(canvas.2),
        by1.min(canvas.3),
    )
}

/// Rasterize glyphs + underlines from `buffer` into `buf` at `(origin_x,
/// origin_y)`, clipped to `clip`. `default_color` is used for glyphs/underlines
/// whose run didn't set an explicit colour.
#[allow(clippy::too_many_arguments)]
fn paint_into(
    buffer: &Buffer,
    engine: &mut TextEngine,
    buf: &mut [u8],
    buf_w: i32,
    buf_h: i32,
    origin_x: i32,
    origin_y: i32,
    clip: (i32, i32, i32, i32),
    default_color: Color,
) {
    let default_color = ct_color(default_color);
    let TextEngine {
        font_system,
        swash_cache,
    } = engine;

    buffer.draw(font_system, swash_cache, default_color, |x, y, w, h, color| {
        blend_rect(buf, buf_w, buf_h, origin_x + x, origin_y + y, w, h, color, clip);
    });

    // Underlines: glyphs whose metadata bit is set, drawn at the baseline.
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            if glyph.metadata & 1 == 0 {
                continue;
            }
            let size = glyph.font_size.max(1.0);
            let thickness = (size / 14.0).round().max(1.0) as u32;
            let uy = origin_y + (run.line_y + size / 8.0).round() as i32;
            let ux = origin_x + glyph.x.round() as i32;
            let color = glyph.color_opt.unwrap_or(default_color);
            blend_rect(buf, buf_w, buf_h, ux, uy, glyph.w.round().max(1.0) as u32, thickness, color, clip);
        }
    }
}

/// Premultiplied sRGB OVER of a coverage `color` (alpha = coverage) over the
/// rect at `(x0, y0)` of size `w x h`, clipped to both `buf` and `clip`.
#[allow(clippy::too_many_arguments)]
fn blend_rect(
    buf: &mut [u8],
    buf_w: i32,
    buf_h: i32,
    x0: i32,
    y0: i32,
    w: u32,
    h: u32,
    color: CtColor,
    clip: (i32, i32, i32, i32),
) {
    let a = u32::from(color.a());
    if a == 0 {
        return;
    }
    // Premultiplied source channels (stored BGRA).
    let sb = (u32::from(color.b()) * a + 127) / 255;
    let sg = (u32::from(color.g()) * a + 127) / 255;
    let sr = (u32::from(color.r()) * a + 127) / 255;
    let inv = 255 - a;

    let x_start = x0.max(clip.0).max(0);
    let y_start = y0.max(clip.1).max(0);
    let x_end = (x0 + w as i32).min(clip.2).min(buf_w);
    let y_end = (y0 + h as i32).min(clip.3).min(buf_h);

    for py in y_start..y_end {
        for px in x_start..x_end {
            let idx = ((py * buf_w + px) * 4) as usize;
            buf[idx] = (sb + (u32::from(buf[idx]) * inv + 127) / 255) as u8;
            buf[idx + 1] = (sg + (u32::from(buf[idx + 1]) * inv + 127) / 255) as u8;
            buf[idx + 2] = (sr + (u32::from(buf[idx + 2]) * inv + 127) / 255) as u8;
            buf[idx + 3] = (a + (u32::from(buf[idx + 3]) * inv + 127) / 255) as u8;
        }
    }
}

/// Our opaque sRGB colour as an opaque cosmic-text colour.
pub(crate) fn ct_color(color: Color) -> CtColor {
    CtColor::rgba(color.r, color.g, color.b, 255)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::fonts::TextEngine;
    use crate::text::{FontId, TextBox, TextContent, TextRun, TextStyle};

    fn engine() -> TextEngine {
        TextEngine::new()
    }

    /// Pick any family the system actually has, so shaping produces glyphs.
    fn some_family(engine: &TextEngine) -> String {
        engine
            .available_families()
            .into_iter()
            .next()
            .unwrap_or_else(|| "sans-serif".to_string())
    }

    #[test]
    fn empty_content_renders_transparent() {
        let mut eng = engine();
        let style = TextStyle::new(FontId::new(some_family(&eng)), Color::BLACK);
        let content = TextContent::empty(TextBox::new(50.0, 50.0, 80.0, 30.0, 0.0), ResizeMode::Fixed, style);
        let px = render_text(&content, &mut eng, 100, 100);
        assert_eq!(px.len(), 100 * 100 * 4);
        assert!(px.iter().all(|&b| b == 0), "empty text must be fully transparent");
    }

    #[test]
    fn non_empty_content_marks_some_pixels() {
        let mut eng = engine();
        let fam = some_family(&eng);
        if eng.available_families().is_empty() {
            return; // headless box with no fonts; nothing to assert
        }
        let style = TextStyle::new(FontId::new(fam), Color::BLACK);
        let content = TextContent {
            box_rect: TextBox::new(100.0, 50.0, 180.0, 40.0, 0.0),
            resize: ResizeMode::Fixed,
            h_align: HAlign::Left,
            v_align: VAlign::Top,
            runs: vec![TextRun::new("Hello", style.clone())],
            default_style: style,
            scale: (1.0, 1.0),
        };
        let px = render_text(&content, &mut eng, 200, 100);
        let opaque = px.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(opaque > 0, "expected some rasterized glyph pixels");
    }

    #[test]
    fn render_text_applies_scale() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let style = TextStyle::new(FontId::new(some_family(&eng)), Color::BLACK);
        let make = |scale: (f32, f32)| TextContent {
            box_rect: TextBox::new(160.0, 60.0, 180.0, 40.0, 0.0),
            resize: ResizeMode::Fixed,
            h_align: HAlign::Left,
            v_align: VAlign::Top,
            runs: vec![TextRun::new("Hello", style.clone())],
            default_style: style.clone(),
            scale,
        };
        let (cw, ch) = (360u32, 140u32);

        // Horizontal opaque extent of the rasterised glyphs.
        let extent = |px: &[u8]| -> Option<u32> {
            let mut min_x = u32::MAX;
            let mut max_x = 0u32;
            for y in 0..ch {
                for x in 0..cw {
                    if px[((y * cw + x) * 4 + 3) as usize] > 0 {
                        min_x = min_x.min(x);
                        max_x = max_x.max(x);
                    }
                }
            }
            (min_x <= max_x).then_some(max_x - min_x)
        };

        let natural = extent(&render_text(&make((1.0, 1.0)), &mut eng, cw, ch))
            .expect("natural glyph pixels");
        let squished = extent(&render_text(&make((0.5, 1.0)), &mut eng, cw, ch))
            .expect("squished glyph pixels");
        // Half-width squish must roughly halve the glyph extent (allow slack
        // for bilinear edges and rounding).
        assert!(
            (squished as f32) < (natural as f32) * 0.7,
            "squished extent {squished} should be well under natural {natural}"
        );
    }

    #[test]
    fn render_visible_local_sizes_to_scaled_box() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let style = TextStyle::new(FontId::new(some_family(&eng)), Color::BLACK);
        let content = TextContent {
            box_rect: TextBox::new(100.0, 50.0, 180.0, 40.0, 0.0),
            resize: ResizeMode::Fixed,
            h_align: HAlign::Left,
            v_align: VAlign::Top,
            runs: vec![TextRun::new("Hello", style.clone())],
            default_style: style,
            scale: (1.0, 1.0),
        };
        // Uniform x2: the local buffer is the visible (2x) size and has glyphs.
        let (px, w, h) = render_visible_local(&content, 2.0, 2.0, &mut eng);
        assert_eq!((w, h), (360, 80));
        assert_eq!(px.len(), (w as usize) * (h as usize) * 4);
        assert!(px.chunks_exact(4).any(|p| p[3] > 0), "expected glyph pixels");
        // Anamorphic (wide) stretch still yields the visible box dims.
        let (_px, w, h) = render_visible_local(&content, 3.0, 1.0, &mut eng);
        assert_eq!((w, h), (540, 40));
    }

    #[test]
    fn measure_grows_with_more_text() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let style = TextStyle::new(FontId::new(some_family(&eng)), Color::BLACK);
        let short = TextContent {
            box_rect: TextBox::new(0.0, 0.0, 0.0, 0.0, 0.0),
            resize: ResizeMode::AutoWidth,
            h_align: HAlign::Left,
            v_align: VAlign::Top,
            runs: vec![TextRun::new("i", style.clone())],
            default_style: style.clone(),
            scale: (1.0, 1.0),
        };
        let long = TextContent {
            runs: vec![TextRun::new("iiiiiiiiiiiiiiii", style.clone())],
            ..short.clone()
        };
        let (sw, _) = measure(&short, &mut eng, None);
        let (lw, _) = measure(&long, &mut eng, None);
        assert!(lw > sw, "longer text should measure wider ({lw} > {sw})");
    }
}
