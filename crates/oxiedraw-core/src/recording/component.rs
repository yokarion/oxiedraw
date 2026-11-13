//! Timelapse frames of a component being edited, drawn where its instances sit
//! on the main canvas while the canvas itself shows the component.

use std::sync::OnceLock;

use oxiedraw_utils::color::{linear_to_srgb, srgb_to_linear};
use oxiedraw_utils::geometry::{Size, TransformFilter, TransformRect};
use oxiedraw_utils::pixels::transform_bgra8;

use super::codec::Frame;
use crate::components::blend_premul_linear;
use crate::document::{BlendMode, Placement};

const ENCODE_STEPS: usize = 65_535;

#[derive(Debug, Clone, Copy)]
pub struct InstancePart {
    pub placement: Placement,
    pub blend: BlendMode,
    pub opacity: f32,
}

/// One step of the main stack above the backdrop, bottom-up.
#[derive(Debug, Clone)]
pub enum StackPart {
    Instance(InstancePart),
    /// Normal layers composited together, canvas-sized premultiplied BGRA8.
    Layers(Vec<u8>),
}

/// The main canvas split around one component's instances.
#[derive(Debug, Clone)]
pub struct ComponentBackdrop {
    size: Size,
    component_size: Size,
    composite: Vec<u8>,
    below: Vec<u8>,
    parts: Vec<StackPart>,
    scaled: Option<(u8, Frame)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Region {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl ComponentBackdrop {
    /// `composite` is the whole main canvas, `below` the layers under the
    /// lowest instance. All buffers are `size`-sized premultiplied BGRA8.
    #[must_use]
    pub const fn new(
        size: Size,
        component_size: Size,
        composite: Vec<u8>,
        below: Vec<u8>,
        parts: Vec<StackPart>,
    ) -> Self {
        Self { size, component_size, composite, below, parts, scaled: None }
    }

    /// The main canvas at `halvings`, with `master` in every instance.
    pub fn frame(&mut self, master: &[u8], master_size: Size, halvings: u8) -> Frame {
        let step = 1u32 << halvings;
        let (out_w, out_h, mut pixels) = {
            let base = self.scaled_composite(halvings);
            (base.width, base.height, base.pixels.clone())
        };
        if master.len() != (master_size.width * master_size.height * 4) as usize {
            return Frame { width: out_w, height: out_h, pixels };
        }
        // Keeps the instance's on-canvas scale when the component was resized.
        let scale = (
            ratio(master_size.width, self.component_size.width),
            ratio(master_size.height, self.component_size.height),
        );
        let limit = (self.size.width / step * step, self.size.height / step * step);
        let original = centered_rect(master_size);
        // Each instance is resampled once, over its own box only.
        let instances: Vec<Option<(Region, Vec<u8>)>> = self
            .parts
            .iter()
            .map(|part| {
                let StackPart::Instance(inst) = part else { return None };
                let mut rect = scaled_rect(inst.placement.to_rect(), scale);
                let area = bounds(&[inst.placement.to_rect(), rect], step, limit)?;
                rect.cx -= to_f32(area.x);
                rect.cy -= to_f32(area.y);
                let pixels = transform_bgra8(
                    master,
                    master_size.width,
                    master_size.height,
                    area.w,
                    area.h,
                    original,
                    rect,
                    TransformFilter::Bilinear,
                );
                Some((area, pixels))
            })
            .collect();
        let row_bytes = out_w as usize * 4;
        for &(region, _) in instances.iter().flatten() {
            let first = (region.y / step) as usize * row_bytes;
            let rows = &mut pixels[first..first + (region.h / step) as usize * row_bytes];
            in_bands(rows, row_bytes, |out_row, out| {
                let band = Region {
                    x: region.x,
                    y: region.y + to_u32(out_row) * step,
                    w: region.w,
                    h: to_u32(out.len() / row_bytes) * step,
                };
                let acc = self.compose(band, &instances);
                downscale_into(band.w, band.h, region.x / step, step, out, out_w, |i| acc[i]);
            });
        }
        Frame { width: out_w, height: out_h, pixels }
    }

    fn scaled_composite(&mut self, halvings: u8) -> &Frame {
        if self.scaled.as_ref().is_none_or(|(h, _)| *h != halvings) {
            let step = 1u32 << halvings;
            let (mut out_w, mut out_h) = (self.size.width, self.size.height);
            for _ in 0..halvings {
                out_w = (out_w / 2).max(1);
                out_h = (out_h / 2).max(1);
            }
            let pixels = if halvings == 0 {
                self.composite.clone()
            } else {
                let mut pixels = vec![0u8; (out_w * out_h * 4) as usize];
                let (row_bytes, width) = (out_w as usize * 4, self.size.width as usize);
                let source_w = self.size.width / step * step;
                let rows = (self.size.height / step) as usize * row_bytes;
                let composite = &self.composite;
                in_bands(&mut pixels[..rows], row_bytes, |out_row, out| {
                    let top = out_row * step as usize;
                    let band_h = to_u32(out.len() / row_bytes) * step;
                    downscale_into(source_w, band_h, 0, step, out, out_w, |i| {
                        let (row, col) = (top + i / source_w as usize, i % source_w as usize);
                        let o = (row * width + col) * 4;
                        decode(&composite[o..o + 4], 1.0)
                    });
                });
                pixels
            };
            self.scaled = Some((halvings, Frame { width: out_w, height: out_h, pixels }));
        }
        &self.scaled.as_ref().expect("filled above").1
    }

    // Premultiplied linear pixels of `band`, the stack recomposited.
    fn compose(&self, band: Region, instances: &[Option<(Region, Vec<u8>)>]) -> Vec<[f32; 4]> {
        let canvas = Region { x: 0, y: 0, w: self.size.width, h: self.size.height };
        let mut acc: Vec<[f32; 4]> = Vec::with_capacity(band.w as usize * band.h as usize);
        for row in region_rows(&self.below, canvas, band) {
            acc.extend(row.chunks_exact(4).map(|px| decode(px, 1.0)));
        }
        for (part, instance) in self.parts.iter().zip(instances) {
            match (part, instance) {
                (StackPart::Instance(inst), Some((area, pixels))) => {
                    blend_into(&mut acc, band, pixels, *area, inst.opacity.clamp(0.0, 1.0), inst.blend);
                }
                (StackPart::Layers(layers), _) => blend_into(&mut acc, band, layers, canvas, 1.0, BlendMode::Normal),
                (StackPart::Instance(_), None) => {}
            }
        }
        acc
    }
}

// Blends `src` (covering `area`) into `acc` (covering `band`) where the two overlap.
fn blend_into(acc: &mut [[f32; 4]], band: Region, src: &[u8], area: Region, opacity: f32, blend: BlendMode) {
    let x0 = band.x.max(area.x);
    let y0 = band.y.max(area.y);
    let x1 = (band.x + band.w).min(area.x + area.w);
    let y1 = (band.y + band.h).min(area.y + area.h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let overlap = Region { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
    let (start, w) = ((x0 - band.x) as usize, overlap.w as usize);
    for (y, row) in (y0..y1).zip(region_rows(src, area, overlap)) {
        let offset = (y - band.y) as usize * band.w as usize + start;
        for (dst, px) in acc[offset..offset + w].iter_mut().zip(row.chunks_exact(4)) {
            if px != [0, 0, 0, 0] {
                *dst = blend_premul_linear(*dst, decode(px, opacity), blend);
            }
        }
    }
}

// Rows of `part` from `buffer`, which holds the pixels of `area`.
fn region_rows(buffer: &[u8], area: Region, part: Region) -> impl Iterator<Item = &[u8]> {
    let (width, x, w) = (area.w as usize, (part.x - area.x) as usize, part.w as usize);
    (part.y - area.y..part.y - area.y + part.h).map(move |y| {
        let start = (y as usize * width + x) * 4;
        &buffer[start..start + w * 4]
    })
}

// Runs `f(first_row, rows)` over row bands of `out` on every core.
fn in_bands(out: &mut [u8], row_bytes: usize, f: impl Fn(usize, &mut [u8]) + Sync) {
    let rows = out.len() / row_bytes.max(1);
    if rows == 0 {
        return;
    }
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get).min(rows);
    let per = rows.div_ceil(threads);
    std::thread::scope(|s| {
        for (i, band) in out.chunks_mut(per * row_bytes).enumerate() {
            let f = &f;
            s.spawn(move || f(i * per, band));
        }
    });
}

// Box-filters a `w x h` source (multiples of `step`) into `out` from column
// `out_x`, matching the GPU's 2:1 linear blits on the sRGB capture images.
fn downscale_into(w: u32, h: u32, out_x: u32, step: u32, out: &mut [u8], out_w: u32, sample: impl Fn(usize) -> [f32; 4]) {
    let scale = 1.0 / to_f32(step * step);
    let (w, step_px) = (w as usize, step as usize);
    for oy in 0..(h / step) as usize {
        for ox in 0..w / step_px {
            let mut sum = [0.0f32; 4];
            for dy in 0..step_px {
                let base = (oy * step_px + dy) * w + ox * step_px;
                for dx in 0..step_px {
                    let s = sample(base + dx);
                    for c in 0..4 {
                        sum[c] += s[c];
                    }
                }
            }
            let o = (oy * out_w as usize + out_x as usize + ox) * 4;
            if let Some(dst) = out.get_mut(o..o + 4) {
                dst.copy_from_slice(&encode(sum.map(|c| c * scale)));
            }
        }
    }
}

// Pixel box covering every rect, padded for bilinear taps and aligned to `step`.
fn bounds(rects: &[TransformRect], step: u32, limit: (u32, u32)) -> Option<Region> {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for rect in rects {
        let (hw, hh) = (rect.half_w().abs(), rect.half_h().abs());
        for (lx, ly) in [(-hw, -hh), (hw, -hh), (-hw, hh), (hw, hh)] {
            let (x, y) = rect.local_to_canvas(lx, ly);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    let start = |v: f32| to_px(v.floor() - 1.0) / step * step;
    let end = |v: f32, max: u32| to_px(v.ceil() + 1.0).div_ceil(step).saturating_mul(step).min(max);
    let (left, top) = (start(x0), start(y0));
    let (right, bottom) = (end(x1, limit.0), end(y1, limit.1));
    (right > left && bottom > top).then_some(Region { x: left, y: top, w: right - left, h: bottom - top })
}

fn decode(px: &[u8], opacity: f32) -> [f32; 4] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    let lut = LUT.get_or_init(|| std::array::from_fn(|i| srgb_to_linear(u8::try_from(i).unwrap_or(u8::MAX))));
    [
        lut[px[0] as usize] * opacity,
        lut[px[1] as usize] * opacity,
        lut[px[2] as usize] * opacity,
        f32::from(px[3]) / 255.0 * opacity,
    ]
}

fn encode(linear: [f32; 4]) -> [u8; 4] {
    static LUT: OnceLock<Vec<u8>> = OnceLock::new();
    #[allow(clippy::cast_precision_loss)]
    let lut = LUT.get_or_init(|| (0..=ENCODE_STEPS).map(|i| linear_to_srgb(i as f32 / ENCODE_STEPS as f32)).collect());
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let channel = |c: f32| lut[(c.clamp(0.0, 1.0) * ENCODE_STEPS as f32).round() as usize];
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let alpha = (linear[3].clamp(0.0, 1.0) * 255.0).round() as u8;
    [channel(linear[0]), channel(linear[1]), channel(linear[2]), alpha]
}

fn centered_rect(size: Size) -> TransformRect {
    let (w, h) = (to_f32(size.width), to_f32(size.height));
    TransformRect::new(w / 2.0, h / 2.0, w, h, 0.0)
}

fn scaled_rect(rect: TransformRect, (sx, sy): (f32, f32)) -> TransformRect {
    TransformRect::new(rect.cx, rect.cy, rect.w * sx, rect.h * sy, rect.angle)
}

fn ratio(now: u32, before: u32) -> f32 {
    if before == 0 { 1.0 } else { to_f32(now) / to_f32(before) }
}

#[allow(clippy::cast_precision_loss)]
const fn to_f32(v: u32) -> f32 {
    v as f32
}

fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_px(v: f32) -> u32 {
    v.max(0.0) as u32
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::components::render_instance;

    fn solid(size: Size, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((size.width * size.height) as usize)
    }

    fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
        let o = ((y * frame.width + x) * 4) as usize;
        frame.pixels[o..o + 4].try_into().unwrap()
    }

    // An 8x8 canvas, white below a 4x4 red instance centred on it.
    fn backdrop(parts_above: Vec<StackPart>) -> (ComponentBackdrop, Vec<u8>) {
        let size = Size::new(8, 8);
        let comp = Size::new(4, 4);
        let master = solid(comp, [0, 0, 255, 255]);
        let placement = Placement::new(4.0, 4.0, 4.0, 4.0, 0.0);
        let instance = render_instance(&master, 4, 4, 8, 8, placement, TransformFilter::Bilinear);
        let below = solid(size, [255, 255, 255, 255]);
        let mut composite = below.clone();
        for (dst, src) in composite.chunks_exact_mut(4).zip(instance.chunks_exact(4)) {
            if src[3] == 255 {
                dst.copy_from_slice(src);
            }
        }
        let mut parts = vec![StackPart::Instance(InstancePart { placement, blend: BlendMode::Normal, opacity: 1.0 })];
        parts.extend(parts_above);
        (ComponentBackdrop::new(size, comp, composite.clone(), below, parts), composite)
    }

    #[test]
    fn unchanged_master_reproduces_the_canvas() {
        let (mut backdrop, composite) = backdrop(Vec::new());
        let frame = backdrop.frame(&solid(Size::new(4, 4), [0, 0, 255, 255]), Size::new(4, 4), 0);
        assert_eq!((frame.width, frame.height), (8, 8));
        assert_eq!(frame.pixels, composite);
    }

    #[test]
    fn edited_master_shows_in_the_instance_only() {
        let (mut backdrop, _) = backdrop(Vec::new());
        let frame = backdrop.frame(&solid(Size::new(4, 4), [255, 0, 0, 255]), Size::new(4, 4), 0);
        assert_eq!(pixel(&frame, 4, 4), [255, 0, 0, 255]);
        assert_eq!(pixel(&frame, 0, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn layers_above_cover_the_instance() {
        let green = solid(Size::new(8, 8), [0, 255, 0, 255]);
        let (mut backdrop, _) = backdrop(vec![StackPart::Layers(green)]);
        let frame = backdrop.frame(&solid(Size::new(4, 4), [255, 0, 0, 255]), Size::new(4, 4), 0);
        assert_eq!(pixel(&frame, 4, 4), [0, 255, 0, 255]);
    }

    #[test]
    fn halvings_shrink_the_frame() {
        let (mut backdrop, _) = backdrop(Vec::new());
        let frame = backdrop.frame(&solid(Size::new(4, 4), [255, 0, 0, 255]), Size::new(4, 4), 1);
        assert_eq!((frame.width, frame.height), (4, 4));
        assert_eq!(pixel(&frame, 2, 2), [255, 0, 0, 255]);
        assert_eq!(pixel(&frame, 0, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn resized_component_keeps_its_scale() {
        let (mut backdrop, _) = backdrop(Vec::new());
        let frame = backdrop.frame(&solid(Size::new(8, 8), [255, 0, 0, 255]), Size::new(8, 8), 0);
        assert_eq!(pixel(&frame, 0, 0), [255, 0, 0, 255]);
    }
}
