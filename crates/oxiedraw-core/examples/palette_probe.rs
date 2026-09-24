//! Run the palette extractor over PNG files and print what comes out.
//!
//! Tuning aid, not part of the app: `cargo run --example palette_probe -- a.png b.png`.
//! Pass `--detail 0.8` or `--background smooth` to try the knobs.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::time::Instant;

use oxiedraw_core::palettes::{BackgroundMode, ExtractOptions, PaletteSource};

fn main() {
    let mut options = ExtractOptions::default();
    let mut files: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--detail" => options.detail = args.next().and_then(|v| v.parse().ok()).unwrap_or(0.5),
            "--limit" => options.limit = args.next().and_then(|v| v.parse().ok()).unwrap_or(24),
            "--background" => {
                options.background = match args.next().as_deref() {
                    Some("smooth") => BackgroundMode::Smooth,
                    Some("detailed") => BackgroundMode::Detailed,
                    _ => BackgroundMode::Ignore,
                };
            }
            _ => files.push(arg),
        }
    }

    for path in files {
        let Some((bgra, width)) = read_png(&path) else {
            eprintln!("{path}: not a readable PNG");
            continue;
        };
        let started = Instant::now();
        let source = PaletteSource::analyze(&bgra, width, None);
        let analyzed = started.elapsed();
        let picked = Instant::now();
        let colors = source.select(&options);
        let selected = picked.elapsed();

        let hexes: Vec<String> = colors
            .iter()
            .map(|c| format!("#{:02X}{:02X}{:02X}", c.color.r, c.color.g, c.color.b))
            .collect();
        println!(
            "{path}: {} colors in {:.0} ms analyze + {:.2} ms select\n  {}",
            colors.len(),
            analyzed.as_secs_f64() * 1000.0,
            selected.as_secs_f64() * 1000.0,
            hexes.join(" ")
        );
    }
}

/// Decode to the premultiplied BGRA8 the canvas hands the extractor.
fn read_png(path: &str) -> Option<(Vec<u8>, u32)> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(file);
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut raw = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut raw).ok()?;

    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => return None,
    };
    let mut bgra = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    for px in raw[..info.buffer_size()].chunks_exact(channels) {
        let (r, g, b, a) = match channels {
            4 => (px[0], px[1], px[2], px[3]),
            3 => (px[0], px[1], px[2], 255),
            2 => (px[0], px[0], px[0], px[1]),
            _ => (px[0], px[0], px[0], 255),
        };
        let mul = |c: u8| ((u16::from(c) * u16::from(a)) / 255) as u8;
        bgra.extend_from_slice(&[mul(b), mul(g), mul(r), a]);
    }
    Some((bgra, info.width))
}
