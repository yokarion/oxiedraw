//! Raw pattern bitmap data carried by `BrushFamily::Textured`.
//!
//! `PatternData` is the brush-side representation: tightly-packed
//! premultiplied RGBA8 bytes with no GPU dependency. The renderer's
//! pattern atlas uploads this once per `Rc<PatternData>` identity and
//! caches the resulting slice index.

#[derive(Debug, Clone)]
pub struct PatternData {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl PatternData {
    pub fn new(rgba: Vec<u8>, width: u32, height: u32) -> Self {
        assert_eq!(
            rgba.len(),
            (width as usize)
                .checked_mul(height as usize)
                .and_then(|n| n.checked_mul(4))
                .expect("pattern dims overflow"),
            "rgba length must be width * height * 4",
        );
        Self {
            rgba,
            width,
            height,
        }
    }

    /// Decode a PNG buffer into a premultiplied-RGBA `PatternData`.
    /// Mirrors the loader inside `format::load` so brushes loaded from
    /// archives and patterns picked at runtime through the UI live in
    /// the same colour space.
    pub fn from_png_bytes(bytes: &[u8]) -> Result<Self, String> {
        let decoder = png::Decoder::new(bytes);
        let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        let straight = &buf[..info.buffer_size()];
        let pixel_count = (info.width as usize) * (info.height as usize);
        let mut premul = Vec::with_capacity(pixel_count * 4);
        match info.color_type {
            png::ColorType::Rgba => {
                for chunk in straight.chunks_exact(4) {
                    let a = chunk[3];
                    let f = f32::from(a) / 255.0;
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    {
                        premul.push((f32::from(chunk[0]) * f).round() as u8);
                        premul.push((f32::from(chunk[1]) * f).round() as u8);
                        premul.push((f32::from(chunk[2]) * f).round() as u8);
                        premul.push(a);
                    }
                }
            }
            png::ColorType::Rgb => {
                for chunk in straight.chunks_exact(3) {
                    premul.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xFF]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for chunk in straight.chunks_exact(2) {
                    let v = chunk[0];
                    let a = chunk[1];
                    let f = f32::from(a) / 255.0;
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let p = (f32::from(v) * f).round() as u8;
                    premul.extend_from_slice(&[p, p, p, a]);
                }
            }
            png::ColorType::Grayscale => {
                for &v in straight {
                    premul.extend_from_slice(&[v, v, v, 0xFF]);
                }
            }
            other @ png::ColorType::Indexed => {
                return Err(format!("unsupported PNG color type {other:?}"))
            }
        }
        Ok(Self::new(premul, info.width, info.height))
    }

    /// Seamless chalk grit: dense deposit broken up by fine paper-tooth
    /// holes. Meant to be sampled in canvas space (REPEAT wrap) as a
    /// global grain behind a square-ish tip, so a chalk stroke reveals
    /// one continuous, non-repeating texture instead of a stamped bump.
    #[allow(clippy::cast_precision_loss)]
    pub fn chalk_grain(dim: u32) -> Self {
        Self::from_value_fn(dim, |px, py| {
            let base = fbm(px, py, dim, 16, 4, 0x51ed_2a17);
            let grit = white_noise(px as i32, py as i32, dim as i32, 0x9e37_79b1);
            // Bias high (dense chalk) with speckly dropouts from the grit.
            smoothstep(0.32, 0.9, 0.55f32.mul_add(base, 0.75 * grit))
        })
    }

    /// Seamless comic-book halftone: a regular grid of solid dots on a
    /// transparent field. Tiles cleanly (`dim` divides into whole cells).
    /// Sampled in canvas space so the dot grid is global and continuous.
    #[allow(clippy::cast_precision_loss)]
    pub fn halftone(dim: u32) -> Self {
        let cells = 8u32;
        let cell = (dim / cells).max(1) as f32;
        let radius = cell * 0.34;
        Self::from_value_fn(dim, |px, py| {
            let cx = (px as f32).rem_euclid(cell) - cell * 0.5 + 0.5;
            let cy = (py as f32).rem_euclid(cell) - cell * 0.5 + 0.5;
            let d = cx.hypot(cy);
            1.0 - smoothstep(radius - 1.5, radius + 0.5, d)
        })
    }

    /// Seamless soft wash: smooth, low-frequency coverage variation with
    /// no grain or high-frequency noise. Stays mostly opaque (values in
    /// ~0.55..1.0) so, behind a soft round tip at reduced opacity + build-
    /// up, it reads as a smooth semi-transparent brush with gentle pooling
    /// wobbles rather than a textured/gritty stroke.
    #[allow(clippy::cast_precision_loss)]
    pub fn soft_wash(dim: u32) -> Self {
        Self::from_value_fn(dim, |px, py| {
            // Periods 4 & 8 -> very large (64..128-texel) soft features.
            let n = fbm(px, py, dim, 4, 2, 0x77a1_20e5);
            0.55 + 0.45 * smoothstep(0.2, 0.8, n)
        })
    }

    /// Build a square `dim` pattern from a per-pixel coverage function
    /// returning `0..=1`. The value is written to all channels (the
    /// grain shaders read alpha; premultiplied white keeps it valid for
    /// any path that reads colour).
    fn from_value_fn(dim: u32, f: impl Fn(u32, u32) -> f32) -> Self {
        let dim_usize = dim as usize;
        let mut rgba = vec![0u8; dim_usize * dim_usize * 4];
        for y in 0..dim {
            for x in 0..dim {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let a = (f(x, y).clamp(0.0, 1.0) * 255.0) as u8;
                let i = (y as usize * dim_usize + x as usize) * 4;
                rgba[i] = a;
                rgba[i + 1] = a;
                rgba[i + 2] = a;
                rgba[i + 3] = a;
            }
        }
        Self::new(rgba, dim, dim)
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * 2.0f32.mul_add(-t, 3.0)
}

fn hash_u32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^= h >> 16;
    h
}

/// White noise on the integer lattice, wrapped to `period` so it tiles.
#[allow(clippy::cast_precision_loss)]
fn white_noise(xi: i32, yi: i32, period: i32, seed: u32) -> f32 {
    #[allow(clippy::cast_sign_loss)]
    let px = xi.rem_euclid(period) as u32;
    #[allow(clippy::cast_sign_loss)]
    let py = yi.rem_euclid(period) as u32;
    let h = hash_u32(
        px.wrapping_mul(374_761_393)
            .wrapping_add(py.wrapping_mul(668_265_263))
            .wrapping_add(seed),
    );
    ((h >> 8) as f32) / ((1u32 << 24) as f32)
}

/// Periodic value noise. `period` lattice cells wrap over the tile, so
/// sampling a pixel at `coord * period / dim` produces noise that is
/// seamless across a `dim`-sized REPEAT tile.
#[allow(clippy::cast_precision_loss)]
fn value_noise(x: f32, y: f32, period: i32, seed: u32) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    let x0 = x.floor() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let y0 = y.floor() as i32;
    let tx = x - x0 as f32;
    let ty = y - y0 as f32;
    let sx = smoothstep(0.0, 1.0, tx);
    let sy = smoothstep(0.0, 1.0, ty);
    let c00 = white_noise(x0, y0, period, seed);
    let c10 = white_noise(x0 + 1, y0, period, seed);
    let c01 = white_noise(x0, y0 + 1, period, seed);
    let c11 = white_noise(x0 + 1, y0 + 1, period, seed);
    let a = c10.mul_add(sx, c00 * (1.0 - sx));
    let b = c11.mul_add(sx, c01 * (1.0 - sx));
    b.mul_add(sy, a * (1.0 - sy))
}

/// Fractal (summed-octave) value noise that tiles over a `dim` pattern.
/// `base_period` doubles each octave; keep `base_period * 2^(octaves-1)`
/// a divisor of `dim` (powers of two) so every octave stays seamless.
#[allow(clippy::cast_precision_loss)]
fn fbm(px: u32, py: u32, dim: u32, base_period: i32, octaves: u32, seed: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 0.5;
    let mut period = base_period;
    for o in 0..octaves {
        let scale = period as f32 / dim as f32;
        let lx = px as f32 * scale;
        let ly = py as f32 * scale;
        sum += amp * value_noise(lx, ly, period, seed.wrapping_add(o.wrapping_mul(0x1013)));
        amp *= 0.5;
        period *= 2;
    }
    sum
}
