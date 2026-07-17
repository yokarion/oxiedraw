//! Bucket-fill flood algorithm + per-frame paint-spill builder.
//!
//! `flood_fill` does a layered 4-neighbour BFS from a seed pixel,
//! then re-sorts the matched pixel indices by **Euclidean distance
//! from the seed** (bucket sort, O(n)). The UI animation paints a
//! growing prefix of that list each frame so pixels reveal outward
//! as a circle - like spilled paint spreading - instead of the
//! Manhattan-distance diamond a raw BFS order would produce.
//!
//! Working set is `Vec<AtomicU8>` (1 byte/pixel, layout-compatible
//! with `Vec<u8>` for cheap zero-init) + `Vec<u32>` of matched
//! indices. The `AtomicU8` visited mask lets large BFS layers be
//! processed in parallel via `std::thread::scope` - each thread
//! claims unvisited pixels with `compare_exchange` so duplicates
//! never appear in the result. Small layers fall back to sequential
//! processing to avoid spawn overhead.

/// Result of a flood fill.
///
/// `sorted_indices[i]` is a linear pixel index (`y*w + x`); the list
/// is ordered by Euclidean distance from the seed, so painting a
/// prefix `sorted_indices[..k]` always yields a (roughly) circular
/// region growing outward from the click point.
///
/// `distance_mask[i]` is a per-pixel byte giving normalised Euclidean
/// distance from the seed in the range 0..=254 (so 255 can be used as
/// a sentinel "not in fill"). The GPU overlay animation samples this
/// as an `R8_UNORM` texture and compares against a running radius push
/// constant - that's how the spread animates with no per-frame layer
/// upload.
pub struct FillResult {
    pub sorted_indices: Vec<u32>,
    pub distance_mask: Vec<u8>,
}

/// Run a flood fill from `(sx, sy)` over BGRA8 `pixels` (row-major,
/// `w x h`). Returns the matched pixel indices in BFS order, or
/// `None` if the seed is out of bounds. Tolerance is interpreted as
/// the maximum sum-of-squares channel difference (B,G,R,A) from the
/// seed pixel; 0 selects only exact-colour neighbours.
///
/// `mask`, when `Some`, is a canvas-sized R8 selection mask (row-major,
/// one byte per pixel). The fill is confined to pixels whose mask byte
/// is non-zero: out-of-selection pixels act as walls, and a seed
/// outside the selection produces an empty fill. `None` means unbounded.
///
/// Large BFS layers (`>= PARALLEL_THRESHOLD` pixels) are processed
/// in parallel via `std::thread::scope`; small layers run on the
/// caller's thread to avoid spawn overhead.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn flood_fill(
    pixels: &[u8],
    w: u32,
    h: u32,
    sx: i32,
    sy: i32,
    tolerance: u8,
    mask: Option<&[u8]>,
) -> Option<FillResult> {
    use std::sync::atomic::{AtomicU8, Ordering};

    if sx < 0 || sy < 0 {
        return None;
    }
    let (ux, uy) = (sx as u32, sy as u32);
    if ux >= w || uy >= h {
        return None;
    }
    let n = (w as usize).checked_mul(h as usize)?;
    if pixels.len() < n * 4 {
        return None;
    }
    // A mask shorter than the canvas is treated as no clip rather than
    // risking an out-of-bounds index during the scan.
    let mask = mask.filter(|m| m.len() >= n);

    let seed_idx = uy * w + ux;

    // Seed outside the selection means there's nothing to fill.
    if let Some(m) = mask
        && m[seed_idx as usize] == 0
    {
        return Some(FillResult {
            sorted_indices: Vec::new(),
            distance_mask: vec![255_u8; n],
        });
    }
    let seed_off = seed_idx as usize * 4;
    let seed = [
        pixels[seed_off],
        pixels[seed_off + 1],
        pixels[seed_off + 2],
        pixels[seed_off + 3],
    ];
    let t = i32::from(tolerance);
    let tol_sq = t * t * 4;

    // Allocate visited as zero-initialised `Vec<u8>` and reinterpret
    // as `Vec<AtomicU8>`. `vec![0u8; n]` goes through `alloc_zeroed`,
    // which on Linux defers the cost to first touch - much faster than
    // the per-element `AtomicU8::new(0)` collect path.
    let visited: Vec<AtomicU8> = {
        let mut zeros = std::mem::ManuallyDrop::new(vec![0u8; n]);
        let ptr = zeros.as_mut_ptr().cast::<AtomicU8>();
        let len = zeros.len();
        let cap = zeros.capacity();
        // SAFETY: AtomicU8 has the same in-memory representation as
        // u8 (per std::sync::atomic docs), same alignment (1), and we
        // use the global allocator on both ends.
        unsafe { Vec::from_raw_parts(ptr, len, cap) }
    };
    let mut sorted: Vec<u32> = Vec::with_capacity(n / 4);

    visited[seed_idx as usize].store(1, Ordering::Relaxed);
    sorted.push(seed_idx);

    let num_threads = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(2)
        .clamp(2, 8);

    let mut layer_start: usize = 0;
    loop {
        let layer_end = sorted.len();
        if layer_start == layer_end {
            break;
        }
        let layer_size = layer_end - layer_start;

        if layer_size < PARALLEL_THRESHOLD {
            // Sequential - too small to amortise thread spawn cost.
            for i in layer_start..layer_end {
                let idx = sorted[i];
                scan_neighbours_seq(idx, w, h, pixels, seed, tol_sq, mask, &visited, &mut sorted);
            }
        } else {
            let chunk_size = layer_size.div_ceil(num_threads);
            // The frontier slice is read-only during this layer; safe
            // to borrow into the scoped threads. Each thread produces
            // a local `Vec<u32>` we drain after the scope.
            let visited_ref: &Vec<AtomicU8> = &visited;
            let chunks: Vec<Vec<u32>> = std::thread::scope(|s| {
                // Collect is needed: all threads must be spawned before
                // any join to get actual parallel execution.
                #[allow(clippy::needless_collect)]
                let handles: Vec<_> = (0..num_threads)
                    .map(|t| {
                        let start = layer_start + t * chunk_size;
                        let end = (layer_start + (t + 1) * chunk_size).min(layer_end);
                        let chunk = &sorted[start..end];
                        s.spawn(move || {
                            let mut local =
                                Vec::with_capacity(chunk.len().saturating_mul(4) / 4);
                            for &idx in chunk {
                                scan_neighbours_atomic(
                                    idx,
                                    w,
                                    h,
                                    pixels,
                                    seed,
                                    tol_sq,
                                    mask,
                                    visited_ref,
                                    &mut local,
                                );
                            }
                            local
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
            });
            for c in chunks {
                sorted.extend_from_slice(&c);
            }
        }
        layer_start = layer_end;
    }

    let (sorted_indices, distance_mask) =
        sort_by_euclidean_and_build_mask(sorted, w, h, sx, sy);
    Some(FillResult {
        sorted_indices,
        distance_mask,
    })
}

/// Minimum BFS-layer size at which parallel processing pays off.
/// Below this we run on the caller's thread to avoid the ~50 us
/// per-thread spawn cost dominating the actual scan work.
const PARALLEL_THRESHOLD: usize = 4096;

/// Single-threaded neighbour scan, mutating `sorted` directly.
#[inline]
fn scan_neighbours_seq(
    idx: u32,
    w: u32,
    h: u32,
    pixels: &[u8],
    seed: [u8; 4],
    tol_sq: i32,
    mask: Option<&[u8]>,
    visited: &[std::sync::atomic::AtomicU8],
    sorted: &mut Vec<u32>,
) {
    use std::sync::atomic::Ordering;
    let y = idx / w;
    let x = idx - y * w;
    let mut try_add = |ni: u32| {
        let nu = ni as usize;
        if visited[nu].load(Ordering::Relaxed) != 0 {
            return;
        }
        if mask.is_some_and(|m| m[nu] == 0) {
            return;
        }
        if !pixel_matches(pixels, nu, seed, tol_sq) {
            return;
        }
        visited[nu].store(1, Ordering::Relaxed);
        sorted.push(ni);
    };
    if y > 0 {
        try_add(idx - w);
    }
    if y + 1 < h {
        try_add(idx + w);
    }
    if x > 0 {
        try_add(idx - 1);
    }
    if x + 1 < w {
        try_add(idx + 1);
    }
}

/// Multi-threaded neighbour scan: claim each neighbour with an
/// atomic CAS so only one thread ever pushes a given pixel.
#[inline]
fn scan_neighbours_atomic(
    idx: u32,
    w: u32,
    h: u32,
    pixels: &[u8],
    seed: [u8; 4],
    tol_sq: i32,
    mask: Option<&[u8]>,
    visited: &[std::sync::atomic::AtomicU8],
    local: &mut Vec<u32>,
) {
    use std::sync::atomic::Ordering;
    let y = idx / w;
    let x = idx - y * w;
    let mut try_claim = |ni: u32| {
        let nu = ni as usize;
        // Quick early-out without CAS for already-claimed pixels.
        if visited[nu].load(Ordering::Relaxed) != 0 {
            return;
        }
        if mask.is_some_and(|m| m[nu] == 0) {
            return;
        }
        if !pixel_matches(pixels, nu, seed, tol_sq) {
            return;
        }
        if visited[nu]
            .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            local.push(ni);
        }
    };
    if y > 0 {
        try_claim(idx - w);
    }
    if y + 1 < h {
        try_claim(idx + w);
    }
    if x > 0 {
        try_claim(idx - 1);
    }
    if x + 1 < w {
        try_claim(idx + 1);
    }
}

/// Re-order matched pixel indices so they're sorted by Euclidean
/// distance from the seed, AND emit a canvas-sized R8 byte mask where
/// each pixel's value encodes its normalised distance (0..=254 across
/// the fill region, 255 = sentinel "not in fill"). The mask is what
/// the GPU overlay shader reads - per-frame animation needs only to
/// update a `reveal_radius` push constant, not re-upload the mask.
///
/// The BFS produces Manhattan-distance order (a diamond); for
/// animation purposes we want a circle. Implemented as a
/// counting/bucket sort on integer-rounded distance - O(n) and
/// avoids the cache thrash of a comparison sort on a multi-million
/// element array.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn sort_by_euclidean_and_build_mask(
    indices: Vec<u32>,
    w: u32,
    h: u32,
    sx: i32,
    sy: i32,
) -> (Vec<u32>, Vec<u8>) {
    let n = indices.len();
    let pixel_count = (w as usize).saturating_mul(h as usize);
    // The mask is canvas-sized; initialise to the sentinel for every
    // pixel and overwrite the fill region in the first pass below.
    let mut distance_mask = vec![255_u8; pixel_count];
    if n == 0 {
        return (indices, distance_mask);
    }
    if n == 1 {
        let idx = indices[0] as usize;
        if idx < distance_mask.len() {
            distance_mask[idx] = 0;
        }
        return (indices, distance_mask);
    }

    // First pass: integer distance per matched pixel + max-distance.
    // Distances fit in u16 for any sane canvas (max ~= sqrt(2.65535^2) > 92681,
    // which is well past Vulkan's maxImageDimension2D limit).
    let mut dists: Vec<u16> = Vec::with_capacity(n);
    let mut max_d: u32 = 0;
    for &idx in &indices {
        let y = idx / w;
        let x = idx - y * w;
        let dx = x as i32 - sx;
        let dy = y as i32 - sy;
        let d2 = (dx * dx + dy * dy) as f32;
        let d = d2.sqrt().round() as u32;
        if d > max_d {
            max_d = d;
        }
        dists.push(d.min(u32::from(u16::MAX)) as u16);
    }

    // Build counts -> prefix-offsets table indexed by integer distance.
    let n_buckets = max_d as usize + 1;
    let mut offsets: Vec<u32> = vec![0; n_buckets];
    for &d in &dists {
        offsets[d as usize] += 1;
    }
    let mut acc: u32 = 0;
    for c in &mut offsets {
        let count = *c;
        *c = acc;
        acc += count;
    }

    // Second pass: place indices into sorted positions and fill the
    // canvas-sized R8 mask with normalised (0..=254) distance.
    let mut sorted = vec![0u32; n];
    let denom = max_d.max(1) as f32;
    for (i, &idx) in indices.iter().enumerate() {
        let d_int = dists[i];
        let pos = offsets[d_int as usize] as usize;
        sorted[pos] = idx;
        offsets[d_int as usize] += 1;
        // Normalise to 0..254; reserve 255 as the "outside fill" sentinel.
        let byte = ((f32::from(d_int) / denom) * 254.0).round() as u32;
        let byte = byte.min(254) as u8;
        if (idx as usize) < distance_mask.len() {
            distance_mask[idx as usize] = byte;
        }
    }
    (sorted, distance_mask)
}

#[inline]
fn pixel_matches(pixels: &[u8], idx: usize, seed: [u8; 4], tol_sq: i32) -> bool {
    let off = idx * 4;
    let db = i32::from(pixels[off]) - i32::from(seed[0]);
    let dg = i32::from(pixels[off + 1]) - i32::from(seed[1]);
    let dr = i32::from(pixels[off + 2]) - i32::from(seed[2]);
    let da = i32::from(pixels[off + 3]) - i32::from(seed[3]);
    (db * db + dg * dg + dr * dr + da * da) <= tol_sq
}

/// Paint `color_bgr` with alpha 255 at every linear pixel index in
/// `indices`, in place. Mutating the working buffer between frames
/// avoids cloning the entire layer each tick.
pub fn paint_indices(buffer: &mut [u8], indices: &[u32], color_bgr: [u8; 3]) {
    for &idx in indices {
        let off = idx as usize * 4;
        if off + 4 > buffer.len() {
            continue;
        }
        buffer[off] = color_bgr[0];
        buffer[off + 1] = color_bgr[1];
        buffer[off + 2] = color_bgr[2];
        buffer[off + 3] = 255;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-transparent 5x5 canvas seeded in the middle: every
    /// pixel is reached, the seed comes first, and the result is
    /// ordered by Euclidean distance (non-decreasing). The far
    /// corners are the last pixels in the list.
    #[test]
    fn fill_orders_by_euclidean_distance() {
        let w = 5u32;
        let h = 5u32;
        let pixels = vec![0u8; (w * h * 4) as usize];
        let (sx, sy) = (2, 2);
        let r = flood_fill(&pixels, w, h, sx, sy, 0, None).expect("seed in bounds");
        assert_eq!(r.sorted_indices.len(), 25, "every pixel matches");
        assert_eq!(r.sorted_indices[0], (sy as u32) * w + (sx as u32));

        let mut prev = 0_u32;
        for &idx in &r.sorted_indices {
            let y = idx / w;
            let x = idx - y * w;
            let dx = x as i32 - sx;
            let dy = y as i32 - sy;
            let d = ((dx * dx + dy * dy) as f32).sqrt().round() as u32;
            assert!(d >= prev, "non-decreasing distance: prev={prev} d={d}");
            prev = d;
        }

        // The four corners are tied for the maximum Euclidean
        // distance and should occupy the last 4 slots.
        let last4: std::collections::HashSet<u32> =
            r.sorted_indices[21..].iter().copied().collect();
        let corners: std::collections::HashSet<u32> = [0, 4, 5 * 4, 5 * 4 + 4].into_iter().collect();
        assert_eq!(last4, corners, "far corners come last");
    }

    /// (1, 0) and (0, 1) (Euclidean distance 1) must come before
    /// (1, 1) (distance sqrt2 ~= 1.41, rounded to 1) - actually all three
    /// share bucket 1; what matters is that they all come before (2, 0)
    /// (distance 2). Guards the diamond -> circle behaviour.
    #[test]
    fn fill_keeps_circular_band_ordering() {
        let w = 3u32;
        let h = 3u32;
        let pixels = vec![0u8; (w * h * 4) as usize];
        let r = flood_fill(&pixels, w, h, 0, 0, 0, None).expect("seed");
        // First three slots after the seed are the d=1 / d~=1.41 band.
        let band1: std::collections::HashSet<u32> = r.sorted_indices[1..4].iter().copied().collect();
        let expected: std::collections::HashSet<u32> = [1, w, w + 1].into_iter().collect();
        assert_eq!(band1, expected, "d~=1 band comes immediately after seed");
    }

    /// Tolerance=0 only spreads along exactly-matching pixels.
    #[test]
    fn fill_respects_tolerance_zero() {
        let w = 3u32;
        let h = 1u32;
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        // Opaque black at (2,0) - does not match transparent seed.
        pixels[8..12].copy_from_slice(&[0, 0, 0, 255]);
        let r = flood_fill(&pixels, w, h, 0, 0, 0, None).expect("seed");
        assert_eq!(r.sorted_indices, vec![0, 1]);
    }

    /// A selection mask confines the fill: the BFS never crosses an
    /// out-of-selection pixel, so only the masked pixels get filled
    /// even though the whole row is the same colour.
    #[test]
    fn fill_confined_to_selection_mask() {
        let w = 5u32;
        let h = 1u32;
        let pixels = vec![0u8; (w * h * 4) as usize];
        // Select only the first three pixels (x = 0, 1, 2).
        let mask = vec![255u8, 255, 255, 0, 0];
        let r = flood_fill(&pixels, w, h, 0, 0, 0, Some(&mask)).expect("seed");
        let filled: std::collections::HashSet<u32> = r.sorted_indices.into_iter().collect();
        assert_eq!(filled, [0, 1, 2].into_iter().collect());
    }

    /// A seed outside the selection produces no fill at all.
    #[test]
    fn fill_seed_outside_selection_is_empty() {
        let w = 3u32;
        let h = 1u32;
        let pixels = vec![0u8; (w * h * 4) as usize];
        let mask = vec![0u8, 0, 0];
        let r = flood_fill(&pixels, w, h, 1, 0, 0, Some(&mask)).expect("in bounds");
        assert!(r.sorted_indices.is_empty());
    }

    /// `paint_indices` writes premul `[B, G, R, 255]` at each listed
    /// pixel and leaves the others untouched.
    #[test]
    fn paint_indices_writes_color_at_listed_pixels() {
        let mut buf = vec![0u8; 12];
        paint_indices(&mut buf, &[0, 2], [10, 20, 30]);
        assert_eq!(&buf[0..4], &[10, 20, 30, 255]);
        assert_eq!(&buf[4..8], &[0, 0, 0, 0]);
        assert_eq!(&buf[8..12], &[10, 20, 30, 255]);
    }
}
