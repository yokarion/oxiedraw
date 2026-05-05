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

    /// Synthesise a soft circular brush stamp with subtle radial noise
    /// for stage-4 validation. Premultiplied white circle, alpha falls
    /// off smoothly, perturbed by a hash so rotated stamps don't reveal
    /// the underlying radial symmetry.
    pub fn debug_chalk(dim: u32) -> Self {
        let dim_usize = dim as usize;
        let mut rgba = vec![0u8; dim_usize * dim_usize * 4];
        let cx = (dim as f32 - 1.0) * 0.5;
        let cy = (dim as f32 - 1.0) * 0.5;
        let radius = cx;
        for y in 0..dim_usize {
            for x in 0..dim_usize {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let d = dx.hypot(dy) / radius;
                let falloff = (1.0 - d).clamp(0.0, 1.0);
                // Soft cosine ramp + low-amplitude hash noise so the
                // stamp looks like chalk grit.
                let smooth = falloff * falloff * 2.0f32.mul_add(-falloff, 3.0);
                let hash = pseudo_random(x as u32, y as u32);
                let grit = hash.mul_add(-0.35, 1.0);
                let alpha = (smooth * grit).clamp(0.0, 1.0);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let a = (alpha * 255.0) as u8;
                let i = (y * dim_usize + x) * 4;
                rgba[i] = a;
                rgba[i + 1] = a;
                rgba[i + 2] = a;
                rgba[i + 3] = a;
            }
        }
        Self::new(rgba, dim, dim)
    }
}

#[allow(clippy::cast_precision_loss)]
fn pseudo_random(x: u32, y: u32) -> f32 {
    let mut h = x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    ((h >> 8) as f32) / ((1u32 << 24) as f32)
}
