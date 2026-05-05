//! Gaussian blur over straight RGB8 buffers, used by the JPEG/WebP/AVIF
//! encoders for optional pre-blur. Format conversions and resampling live
//! in `oxiedraw_utils::pixels`.

pub(super) fn gaussian_blur_rgb8(pixels: &[u8], w: u32, h: u32, sigma: f32) -> Vec<u8> {
    let radius = (sigma * 3.0).ceil() as i32;
    let kernel: Vec<f32> = {
        let raw: Vec<f32> = (-radius..=radius)
            .map(|i| (-0.5 * (i as f32 / sigma).powi(2)).exp())
            .collect();
        let sum: f32 = raw.iter().sum();
        raw.iter().map(|v| v / sum).collect()
    };

    let (wu, hu) = (w as usize, h as usize);
    let mut tmp = vec![0u8; wu * hu * 3];
    let mut out = vec![0u8; wu * hu * 3];

    // Horizontal pass
    for y in 0..hu {
        for x in 0..wu {
            let mut r = 0.0_f32;
            let mut g = 0.0_f32;
            let mut b = 0.0_f32;
            for (ki, kx) in (-radius..=radius).enumerate() {
                let sx = (x as i32 + kx).max(0).min(w as i32 - 1) as usize;
                let pi = (y * wu + sx) * 3;
                r += f32::from(pixels[pi]) * kernel[ki];
                g += f32::from(pixels[pi + 1]) * kernel[ki];
                b += f32::from(pixels[pi + 2]) * kernel[ki];
            }
            let di = (y * wu + x) * 3;
            tmp[di] = r.round() as u8;
            tmp[di + 1] = g.round() as u8;
            tmp[di + 2] = b.round() as u8;
        }
    }

    // Vertical pass
    for y in 0..hu {
        for x in 0..wu {
            let mut r = 0.0_f32;
            let mut g = 0.0_f32;
            let mut b = 0.0_f32;
            for (ki, ky) in (-radius..=radius).enumerate() {
                let sy = (y as i32 + ky).max(0).min(h as i32 - 1) as usize;
                let pi = (sy * wu + x) * 3;
                r += f32::from(tmp[pi]) * kernel[ki];
                g += f32::from(tmp[pi + 1]) * kernel[ki];
                b += f32::from(tmp[pi + 2]) * kernel[ki];
            }
            let di = (y * wu + x) * 3;
            out[di] = r.round() as u8;
            out[di + 1] = g.round() as u8;
            out[di + 2] = b.round() as u8;
        }
    }
    out
}
