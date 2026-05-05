//! Format encoders (PNG, JPEG, WebP, AVIF), preview generator, and PNG
//! decoder. Pixel-format helpers live in the sibling `pixel_format`
//! module.

use std::io::{BufWriter, Write};
use std::path::Path;

use oxiedraw_utils::pixels::{
    premul_bgra8_over_white_to_rgb8, premul_bgra8_to_rgba8, rgb8_to_opaque_bgra8, scale,
    straight_rgba8_to_premul_bgra8,
};

use super::pixel_format::gaussian_blur_rgb8;
use super::settings::{
    AvifSettings, ChromaSubsampling, ExportFormat, ExportSettings, JpegSettings, PngBitDepth,
    PngSettings, WebpSettings,
};

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("png: {0}")]
    Png(#[from] png::EncodingError),
    #[error("jpeg: {0}")]
    Jpeg(String),
    #[error("webp: {0}")]
    Webp(String),
    #[error("avif: {0}")]
    Avif(String),
}

/// Export premultiplied BGRA8 pixels (`canvas_w` x `canvas_h`, row-major)
/// to `path`, applying scale + format-specific encoding from `settings`.
///
/// Safe to call from a worker thread.
pub fn export_pixels(
    bgra8: &[u8],
    canvas_w: u32,
    canvas_h: u32,
    settings: &ExportSettings,
    path: &Path,
) -> Result<(), ExportError> {
    let (scaled, w, h) = scale(bgra8, canvas_w, canvas_h, settings.scale);
    match settings.format {
        ExportFormat::Png => encode_png(&scaled, w, h, &settings.png, path),
        ExportFormat::Jpeg => encode_jpeg(&scaled, w, h, &settings.jpeg, path),
        ExportFormat::Webp => encode_webp(&scaled, w, h, &settings.webp, path),
        ExportFormat::Avif => encode_avif(&scaled, w, h, &settings.avif, path),
    }
}

/// Generate a preview by encoding to the target format in memory and decoding back.
/// The returned pixels are premultiplied BGRA8 for a cairo `ARgb32` surface.
pub fn generate_preview_pixels(
    bgra8: &[u8],
    canvas_w: u32,
    canvas_h: u32,
    settings: &ExportSettings,
) -> (Vec<u8>, u32, u32, bool) {
    let (scaled, w, h) = scale(bgra8, canvas_w, canvas_h, settings.scale);
    let has_alpha_fallback = match settings.format {
        ExportFormat::Png => settings.png.transparency,
        ExportFormat::Webp => settings.webp.transparency,
        ExportFormat::Avif => settings.avif.transparency,
        ExportFormat::Jpeg => false,
    };

    let encoded = match settings.format {
        ExportFormat::Png => encode_png_to_memory(&scaled, w, h, &settings.png),
        ExportFormat::Jpeg => encode_jpeg_to_memory(&scaled, w, h, &settings.jpeg),
        ExportFormat::Webp => encode_webp_to_memory(&scaled, w, h, &settings.webp),
        ExportFormat::Avif => encode_avif_to_memory(&scaled, w, h, &settings.avif),
    };
    let Some(bytes) = encoded else {
        return (scaled, w, h, has_alpha_fallback);
    };

    let decoded = match settings.format {
        ExportFormat::Png => decode_png(&bytes),
        ExportFormat::Jpeg => decode_via_image(&bytes, image::ImageFormat::Jpeg),
        ExportFormat::Webp => decode_via_image(&bytes, image::ImageFormat::WebP),
        ExportFormat::Avif => decode_via_image(&bytes, image::ImageFormat::Avif),
    };
    decoded.unwrap_or((scaled, w, h, has_alpha_fallback))
}

fn encode_png_to_memory(bgra8: &[u8], w: u32, h: u32, s: &PngSettings) -> Option<Vec<u8>> {
    use png::{BitDepth, ColorType, Compression};
    let (color_type, pixel_data) = if s.transparency {
        (ColorType::Rgba, premul_bgra8_to_rgba8(bgra8))
    } else {
        (ColorType::Rgb, premul_bgra8_over_white_to_rgb8(bgra8))
    };
    let final_data: Vec<u8> = match s.bit_depth {
        PngBitDepth::Eight => pixel_data,
        PngBitDepth::Sixteen => pixel_data.iter().flat_map(|&b| [b, b]).collect(),
    };
    let mut buf = Vec::<u8>::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_color(color_type);
        enc.set_compression(match s.compression {
            0..=2 => Compression::Fast,
            3..=5 => Compression::Default,
            _ => Compression::Best,
        });
        enc.set_depth(match s.bit_depth {
            PngBitDepth::Eight => BitDepth::Eight,
            PngBitDepth::Sixteen => BitDepth::Sixteen,
        });
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&final_data).ok()?;
    }
    Some(buf)
}

fn encode_jpeg_to_memory(bgra8: &[u8], w: u32, h: u32, s: &JpegSettings) -> Option<Vec<u8>> {
    use jpeg_encoder::{ColorType, SamplingFactor};
    let mut rgb = premul_bgra8_over_white_to_rgb8(bgra8);
    if s.blur > 0.1 {
        rgb = gaussian_blur_rgb8(&rgb, w, h, s.blur);
    }
    let sampling = match s.chroma_subsampling {
        ChromaSubsampling::Cs444 => SamplingFactor::R_4_4_4,
        ChromaSubsampling::Cs422 => SamplingFactor::R_4_2_2,
        ChromaSubsampling::Cs420 => SamplingFactor::R_4_2_0,
        ChromaSubsampling::Cs411 => SamplingFactor::R_4_1_1,
    };
    let mut buf = Vec::<u8>::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut buf, s.quality);
    enc.set_sampling_factor(sampling);
    enc.set_progressive(s.progressive);
    #[allow(clippy::cast_possible_truncation)]
    enc.encode(&rgb, w as u16, h as u16, ColorType::Rgb).ok()?;
    Some(buf)
}

fn encode_webp_to_memory(bgra8: &[u8], w: u32, h: u32, s: &WebpSettings) -> Option<Vec<u8>> {
    use image::ImageEncoder as _;
    use image::codecs::webp::WebPEncoder;
    let mut buf = Vec::<u8>::new();
    if s.transparency {
        let rgba = premul_bgra8_to_rgba8(bgra8);
        WebPEncoder::new_lossless(&mut buf)
            .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
            .ok()?;
    } else {
        let rgb = premul_bgra8_over_white_to_rgb8(bgra8);
        WebPEncoder::new_lossless(&mut buf)
            .write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
            .ok()?;
    }
    Some(buf)
}

fn encode_avif_to_memory(bgra8: &[u8], w: u32, h: u32, s: &AvifSettings) -> Option<Vec<u8>> {
    let quality = if s.lossless { 100.0 } else { f32::from(s.quality) };
    let speed = s.speed.clamp(1, 10);
    let mut enc = ravif::Encoder::new()
        .with_quality(quality)
        .with_alpha_quality(quality)
        .with_speed(speed);
    if s.lossless {
        enc = enc.with_quality(100.0).with_alpha_quality(100.0);
    }
    let data = if s.transparency {
        let pixels: Vec<rgb::RGBA8> = bgra8
            .chunks_exact(4)
            .map(|p| rgb::RGBA8 {
                r: p[2],
                g: p[1],
                b: p[0],
                a: p[3],
            })
            .collect();
        enc.encode_rgba(ravif::Img::new(&pixels, w as usize, h as usize))
            .ok()?
            .avif_file
    } else {
        let pixels: Vec<rgb::RGB8> = bgra8
            .chunks_exact(4)
            .map(|p| {
                let a = p[3];
                let inv = 255 - a;
                rgb::RGB8 {
                    r: p[2].saturating_add(inv),
                    g: p[1].saturating_add(inv),
                    b: p[0].saturating_add(inv),
                }
            })
            .collect();
        enc.encode_rgb(ravif::Img::new(&pixels, w as usize, h as usize))
            .ok()?
            .avif_file
    };
    Some(data)
}

/// Decode a PNG byte buffer to premultiplied BGRA8 pixels suitable for
/// GPU layer upload. Returns `(pixels, width, height)` or `None` on failure.
pub fn decode_png_bytes(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    decode_png(bytes).map(|(p, w, h, _)| (p, w, h))
}

fn decode_png(buf: &[u8]) -> Option<(Vec<u8>, u32, u32, bool)> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(buf))
        .read_info()
        .ok()?;
    let mut data = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut data).ok()?;
    let sz = info.buffer_size();
    match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => Some((
            straight_rgba8_to_premul_bgra8(&data[..sz]),
            info.width,
            info.height,
            true,
        )),
        (png::ColorType::Rgba, png::BitDepth::Sixteen) => {
            let rgba8: Vec<u8> = data[..sz].chunks_exact(2).map(|c| c[0]).collect();
            Some((
                straight_rgba8_to_premul_bgra8(&rgba8),
                info.width,
                info.height,
                true,
            ))
        }
        (png::ColorType::Rgb, png::BitDepth::Eight) => Some((
            rgb8_to_opaque_bgra8(&data[..sz]),
            info.width,
            info.height,
            false,
        )),
        (png::ColorType::Rgb, png::BitDepth::Sixteen) => {
            let rgb8: Vec<u8> = data[..sz].chunks_exact(2).map(|c| c[0]).collect();
            Some((rgb8_to_opaque_bgra8(&rgb8), info.width, info.height, false))
        }
        _ => None,
    }
}

fn decode_via_image(buf: &[u8], fmt: image::ImageFormat) -> Option<(Vec<u8>, u32, u32, bool)> {
    let img = image::load_from_memory_with_format(buf, fmt).ok()?;
    let has_alpha = img.color().has_alpha();
    let (w, h) = (img.width(), img.height());
    let out = if has_alpha {
        straight_rgba8_to_premul_bgra8(img.to_rgba8().as_raw())
    } else {
        rgb8_to_opaque_bgra8(img.to_rgb8().as_raw())
    };
    Some((out, w, h, has_alpha))
}

/// Rough byte-count estimate for the given canvas + settings.
pub fn estimate_size_bytes(canvas_w: u32, canvas_h: u32, settings: &ExportSettings) -> u64 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let dw = u64::from(((canvas_w as f32 * settings.scale).round() as u32).max(1));
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let dh = u64::from(((canvas_h as f32 * settings.scale).round() as u32).max(1));
    let px = dw * dh;

    match settings.format {
        ExportFormat::Png => {
            let channels: u64 = if settings.png.transparency { 4 } else { 3 };
            let depth_mult: u64 = match settings.png.bit_depth {
                PngBitDepth::Eight => 1,
                PngBitDepth::Sixteen => 2,
            };
            let compress_pct: u64 = match settings.png.compression {
                0..=1 => 60,
                2..=4 => 45,
                5..=6 => 33,
                7..=8 => 23,
                _ => 17,
            };
            px * channels * depth_mult * compress_pct / 100
        }
        ExportFormat::Jpeg => {
            let q = u64::from(settings.jpeg.quality);
            px * 3 * q / 100 / 12
        }
        ExportFormat::Webp => {
            if settings.webp.lossless {
                let ch: u64 = if settings.webp.transparency { 4 } else { 3 };
                px * ch / 3
            } else {
                let q = u64::from(settings.webp.quality);
                let ch: u64 = if settings.webp.transparency { 4 } else { 3 };
                px * ch * q / 100 / 14
            }
        }
        ExportFormat::Avif => {
            if settings.avif.lossless {
                let ch: u64 = if settings.avif.transparency { 4 } else { 3 };
                px * ch / 6
            } else {
                let q = u64::from(settings.avif.quality);
                let ch: u64 = if settings.avif.transparency { 4 } else { 3 };
                px * ch * q / 100 / 20
            }
        }
    }
    .max(1)
}

/// BGRA8 -> straight-alpha RGBA8
fn encode_png(
    bgra8: &[u8],
    w: u32,
    h: u32,
    s: &PngSettings,
    path: &Path,
) -> Result<(), ExportError> {
    use png::{BitDepth, ColorType, Compression};

    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(BufWriter::new(file), w, h);

    let (color_type, pixel_data) = if s.transparency {
        (ColorType::Rgba, premul_bgra8_to_rgba8(bgra8))
    } else {
        (ColorType::Rgb, premul_bgra8_over_white_to_rgb8(bgra8))
    };

    enc.set_color(color_type);
    enc.set_compression(match s.compression {
        0..=2 => Compression::Fast,
        3..=5 => Compression::Default,
        _ => Compression::Best,
    });
    enc.set_depth(match s.bit_depth {
        PngBitDepth::Eight => BitDepth::Eight,
        PngBitDepth::Sixteen => BitDepth::Sixteen,
    });

    let final_data = match s.bit_depth {
        PngBitDepth::Eight => pixel_data,
        PngBitDepth::Sixteen => pixel_data.iter().flat_map(|&b| [b, b]).collect(),
    };

    let mut writer = enc.write_header()?;
    writer.write_image_data(&final_data)?;
    Ok(())
}

fn encode_jpeg(
    bgra8: &[u8],
    w: u32,
    h: u32,
    s: &JpegSettings,
    path: &Path,
) -> Result<(), ExportError> {
    use jpeg_encoder::{ColorType, SamplingFactor};

    let mut rgb = premul_bgra8_over_white_to_rgb8(bgra8);

    if s.blur > 0.1 {
        rgb = gaussian_blur_rgb8(&rgb, w, h, s.blur);
    }

    let sampling = match s.chroma_subsampling {
        ChromaSubsampling::Cs444 => SamplingFactor::R_4_4_4,
        ChromaSubsampling::Cs422 => SamplingFactor::R_4_2_2,
        ChromaSubsampling::Cs420 => SamplingFactor::R_4_2_0,
        ChromaSubsampling::Cs411 => SamplingFactor::R_4_1_1,
    };

    let file = std::fs::File::create(path)?;
    let mut enc = jpeg_encoder::Encoder::new(BufWriter::new(file), s.quality);
    enc.set_sampling_factor(sampling);
    enc.set_progressive(s.progressive);
    #[allow(clippy::cast_possible_truncation)]
    enc.encode(&rgb, w as u16, h as u16, ColorType::Rgb)
        .map_err(|e: jpeg_encoder::EncodingError| ExportError::Jpeg(e.to_string()))?;
    Ok(())
}

fn encode_webp(
    bgra8: &[u8],
    w: u32,
    h: u32,
    s: &WebpSettings,
    path: &Path,
) -> Result<(), ExportError> {
    use image::ImageEncoder as _;
    use image::codecs::webp::WebPEncoder;

    let file = std::fs::File::create(path)?;

    if s.transparency {
        let rgba = premul_bgra8_to_rgba8(bgra8);
        WebPEncoder::new_lossless(file)
            .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
            .map_err(|e: image::ImageError| ExportError::Webp(e.to_string()))?;
    } else {
        let rgb = premul_bgra8_over_white_to_rgb8(bgra8);
        WebPEncoder::new_lossless(file)
            .write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
            .map_err(|e: image::ImageError| ExportError::Webp(e.to_string()))?;
    }
    Ok(())
}

fn encode_avif(
    bgra8: &[u8],
    w: u32,
    h: u32,
    s: &AvifSettings,
    path: &Path,
) -> Result<(), ExportError> {
    let quality = if s.lossless { 100.0 } else { f32::from(s.quality) };
    // ravif speed 1 = slowest/best, 10 = fastest - user's slider: 0(slow)..10(fast)
    let speed = s.speed.clamp(1, 10);

    let mut enc = ravif::Encoder::new()
        .with_quality(quality)
        .with_alpha_quality(quality)
        .with_speed(speed);
    if s.lossless {
        enc = enc.with_quality(100.0).with_alpha_quality(100.0);
    }

    let avif_data = if s.transparency {
        let pixels: Vec<rgb::RGBA8> = bgra8
            .chunks_exact(4)
            .map(|p| rgb::RGBA8 {
                r: p[2],
                g: p[1],
                b: p[0],
                a: p[3],
            })
            .collect();
        enc.encode_rgba(ravif::Img::new(&pixels, w as usize, h as usize))
            .map_err(|e| ExportError::Avif(e.to_string()))?
            .avif_file
    } else {
        let pixels: Vec<rgb::RGB8> = bgra8
            .chunks_exact(4)
            .map(|p| {
                let a = p[3];
                let inv = 255 - a;
                rgb::RGB8 {
                    r: p[2].saturating_add(inv),
                    g: p[1].saturating_add(inv),
                    b: p[0].saturating_add(inv),
                }
            })
            .collect();
        enc.encode_rgb(ravif::Img::new(&pixels, w as usize, h as usize))
            .map_err(|e| ExportError::Avif(e.to_string()))?
            .avif_file
    };

    let mut file = BufWriter::new(std::fs::File::create(path)?);
    file.write_all(&avif_data)?;
    Ok(())
}
