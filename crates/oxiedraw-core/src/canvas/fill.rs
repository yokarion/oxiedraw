//! Bucket-fill flood algorithm + per-frame paint-spill builder.
//!
//! `flood_fill` does a layered 4-neighbour BFS from a seed pixel,
//! then re-sorts the matched pixel indices by **Euclidean distance
//! from the seed** (bucket sort, O(n)). The UI animation paints a
//! growing prefix of that list each frame so pixels reveal outward
//! as a circle - like spilled paint spreading - instead of the
//! Manhattan-distance diamond a raw BFS order would produce.
//!
//! On top of the plain region the fill runs an *edge climb* (see
//! [`FillOptions::auto_edge`]): from the region's boundary it walks
//! outward while each step lands further from the seed colour, which
//! is exactly the anti-aliasing ramp of whatever bounds the region,
//! and stops at the ramp's peak - the line's core. No radius to tune,
//! because the walk terminates on its own.
//!
//! How those edge pixels get painted depends on what was there. Click
//! on empty canvas and the fill goes in *behind* the existing pixels
//! ([`FillPaint::Behind`]), so a line's own anti-aliasing survives
//! untouched and the boundary stays smooth. Click on solid colour and
//! the edge pixels are un-mixed instead: they hold a blend of the seed
//! colour and the line, so swapping the seed's contribution for the
//! fill colour rebuilds the same blend around the new colour.
//!
//! Working set is `Vec<AtomicU8>` (1 byte/pixel, layout-compatible
//! with `Vec<u8>` for cheap zero-init) + `Vec<u32>` of matched
//! indices. The `AtomicU8` visited mask lets large BFS layers be
//! processed in parallel via `std::thread::scope` - each thread
//! claims unvisited pixels with `compare_exchange` so duplicates
//! never appear in the result. Small layers fall back to sequential
//! processing to avoid spawn overhead.

/// Tunables for a bucket fill.
///
/// `tolerance` is the maximum colour difference (0..=255, RMS across
/// the BGRA channels) from the seed pixel that still counts as part of
/// the region.
///
/// `auto_edge` runs the edge climb described in the module docs: the
/// fill carries on across a boundary's anti-aliased ramp and stops at
/// its peak, then paints those pixels in a way that keeps the ramp
/// intact. It needs no tuning and is on by default.
///
/// `reveal_order` produces the extras the reveal animation needs: the
/// matched pixels re-sorted by distance from the seed, and the mask that
/// encodes that distance. Both cost a pass over the whole canvas, so a
/// fill that is going to appear at once - every step of a threshold
/// drag, for instance - asks for it to be skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillOptions {
    pub tolerance: u8,
    pub auto_edge: bool,
    pub reveal_order: bool,
}

impl Default for FillOptions {
    fn default() -> Self {
        Self {
            tolerance: 16,
            auto_edge: true,
            reveal_order: true,
        }
    }
}

/// How the fill colour is combined with what the layer already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillPaint {
    /// The seed was empty canvas, so the fill slides in *underneath*
    /// the existing pixels. Anti-aliased line art keeps its own edge
    /// blending and the fill boundary inherits it for free.
    Behind,
    /// The seed had colour, so the fill replaces the region outright.
    /// Edge pixels are un-mixed: whatever share of them belonged to the
    /// seed colour is handed over to the fill colour.
    Over { seed: [u8; 4] },
}

/// How far the edge climb may travel before giving up. The walk almost
/// always stops after two or three pixels on its own - this only bounds
/// the pathological case of a very soft or blurred boundary.
const EDGE_CLIMB_LIMIT: u8 = 8;

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
///
/// `coverage[i]` is the share of the pixel the fill colour ends up
/// owning (0 = untouched, 255 = all of it). The flat interior is 255;
/// an edge pixel gets whatever the boundary leaves over. It is the same
/// number the GPU overlay uses to hide the fill during the reveal
/// animation, so preview and pixels always agree.
///
/// `paint` says how that share is combined with the layer.
pub struct FillResult {
    pub sorted_indices: Vec<u32>,
    pub distance_mask: Vec<u8>,
    pub coverage: Vec<u8>,
    pub paint: FillPaint,
}

/// The buffers a fill works against, all canvas-sized and row-major.
///
/// `sample` and `target` differ when the tool samples every visible
/// layer: the region is read off the composite while the paint lands in
/// one layer. The distinction matters beyond bookkeeping - where the
/// fill *goes* is a question about the composite, but whether it can
/// slide underneath existing pixels is a question about the target.
pub struct FillSource<'a> {
    /// BGRA8 pixels the region is detected in.
    pub sample: &'a [u8],
    /// BGRA8 pixels the fill will be painted into.
    pub target: &'a [u8],
    /// BGRA8 composite of the layers *above* the target, when there are
    /// any. An edge pixel's boundary can sit above the fill or below it,
    /// and the fill has to be painted differently for each: solid if
    /// something above will blend it, feathered if nothing will. `None`
    /// means the target is the topmost thing the fill has to consider.
    pub occluders: Option<&'a [u8]>,
    /// R8 selection mask, one byte per pixel. The fill is confined to
    /// pixels whose byte is non-zero: out-of-selection pixels act as
    /// walls, and a seed outside the selection produces an empty fill.
    /// `None` means unbounded.
    pub mask: Option<&'a [u8]>,
}

impl<'a> FillSource<'a> {
    /// The common case: detect and paint in the same buffer, with
    /// nothing above it that matters.
    #[must_use]
    pub const fn single(pixels: &'a [u8], mask: Option<&'a [u8]>) -> Self {
        Self {
            sample: pixels,
            target: pixels,
            occluders: None,
            mask,
        }
    }
}

/// Run a flood fill from `(sx, sy)` over a `w x h` canvas. Returns the
/// matched pixel indices in BFS order, or `None` if the seed is out of
/// bounds. See [`FillOptions`] for how tolerance, the edge climb and the
/// manual grow interact.
///
/// Large BFS layers (`>= PARALLEL_THRESHOLD` pixels) are processed
/// in parallel via `std::thread::scope`; small layers run on the
/// caller's thread to avoid spawn overhead.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn flood_fill(
    source: &FillSource,
    w: u32,
    h: u32,
    sx: i32,
    sy: i32,
    opts: FillOptions,
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
    let pixels = source.sample;
    if pixels.len() < n * 4 || source.target.len() < n * 4 {
        return None;
    }
    // A mask shorter than the canvas is treated as no clip rather than
    // risking an out-of-bounds index during the scan.
    let mask = source.mask.filter(|m| m.len() >= n);

    let seed_idx = uy * w + ux;

    // Seed outside the selection means there's nothing to fill.
    if let Some(m) = mask
        && m[seed_idx as usize] == 0
    {
        return Some(FillResult {
            sorted_indices: Vec::new(),
            distance_mask: vec![255_u8; n],
            coverage: vec![0_u8; n],
            paint: FillPaint::Behind,
        });
    }
    let seed_off = seed_idx as usize * 4;
    let seed = [
        pixels[seed_off],
        pixels[seed_off + 1],
        pixels[seed_off + 2],
        pixels[seed_off + 3],
    ];
    let tolerance = opts.tolerance;
    // Whether the fill can slide underneath is a question about the
    // layer being painted, not the one being sampled. Landing on empty
    // pixels means a line's anti-aliased skirt is the only thing there,
    // so going under it leaves that blending alone.
    let target_seed = [
        source.target[seed_off],
        source.target[seed_off + 1],
        source.target[seed_off + 2],
        source.target[seed_off + 3],
    ];
    let paint = if target_seed[3] == 0 {
        FillPaint::Behind
    } else {
        FillPaint::Over { seed: target_seed }
    };

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
                scan_neighbours_seq(idx, w, h, pixels, seed, tolerance, mask, &visited, &mut sorted);
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
                                    tolerance,
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

    // The matched region is the fill's outright: the pixels there are
    // the seed colour, give or take the tolerance.
    let mut coverage = vec![0_u8; n];
    for &idx in &sorted {
        coverage[idx as usize] = 255;
    }

    if opts.auto_edge {
        let band_start = sorted.len();
        let roots = climb_edge(pixels, w, h, seed, mask, &visited, &mut sorted);
        set_edge_coverage(
            EdgeCoverage {
                sample: pixels,
                target: source.target,
                occluders: source.occluders.filter(|o| o.len() >= n * 4),
                seed,
                tolerance,
                paint,
            },
            &sorted[band_start..],
            &roots,
            &mut coverage,
        );
    }

    // Going in underneath means whatever the target already holds at a
    // pixel is that much less room for the fill. Folding it in here
    // leaves `coverage` as the fill's true share of every pixel, which
    // is what both the paint step and the reveal animation want.
    if matches!(paint, FillPaint::Behind) {
        for &idx in &sorted {
            let idx = idx as usize;
            let taken = source.target[idx * 4 + 3];
            if taken != 0 {
                coverage[idx] = mul255(coverage[idx], 255 - taken);
            }
        }
    }

    // Painting doesn't care what order the indices are in, so a fill
    // that isn't going to be swept in skips the re-sort and the
    // canvas-sized mask that only the animation reads.
    let (sorted_indices, distance_mask) = if opts.reveal_order {
        sort_by_euclidean_and_build_mask(sorted, w, h, sx, sy)
    } else {
        (sorted, Vec::new())
    };
    Some(FillResult {
        sorted_indices,
        distance_mask,
        coverage,
        paint,
    })
}

/// Walk outward from the region across a boundary's anti-aliasing ramp.
///
/// Every step must land *further* from the seed colour than the pixel it
/// came from, so the walk runs up the ramp and stops the moment the
/// difference stops rising - the crest, which for line art is the middle
/// of the line. That is what makes the reach automatic: a crisp edge
/// yields one pixel, a soft one yields several, and neither needs a
/// radius from the user. [`EDGE_CLIMB_LIMIT`] only bounds a boundary so
/// blurred it has no clear crest.
///
/// Returns the *root* of each climbed pixel - the region pixel its walk
/// set out from - parallel to the pixels appended to `sorted`. Weighing
/// the band needs it: a region can be bounded by a black outline on one
/// side and a pale one on the other, and those are two different ramps.
fn climb_edge(
    pixels: &[u8],
    w: u32,
    h: u32,
    seed: [u8; 4],
    mask: Option<&[u8]>,
    visited: &[std::sync::atomic::AtomicU8],
    sorted: &mut Vec<u32>,
) -> Vec<u32> {
    use std::sync::atomic::Ordering;

    let band_start = sorted.len();
    let mut roots: Vec<u32> = Vec::new();
    let mut start = 0_usize;
    let mut end = sorted.len();
    for _ in 0..EDGE_CLIMB_LIMIT {
        for i in start..end {
            let idx = sorted[i];
            // The first ring sets out from the region itself; every ring
            // after it inherits whichever boundary pixel started the walk.
            let root = if i >= band_start {
                roots[i - band_start]
            } else {
                idx
            };
            let y = idx / w;
            let x = idx - y * w;
            let x0 = x.saturating_sub(1);
            let x1 = (x + 1).min(w - 1);
            let y0 = y.saturating_sub(1);
            let y1 = (y + 1).min(h - 1);
            // Only worth computing once we know there is somewhere to go.
            let mut here: Option<u32> = None;
            for ny in y0..=y1 {
                for nx in x0..=x1 {
                    let ni = ny * w + nx;
                    let nu = ni as usize;
                    if visited[nu].load(Ordering::Relaxed) != 0 {
                        continue;
                    }
                    if mask.is_some_and(|m| m[nu] == 0) {
                        continue;
                    }
                    let from = *here.get_or_insert_with(|| pixel_diff(pixels, idx as usize, seed));
                    if pixel_diff(pixels, nu, seed) <= from {
                        continue;
                    }
                    visited[nu].store(1, Ordering::Relaxed);
                    sorted.push(ni);
                    roots.push(root);
                }
            }
        }
        start = end;
        end = sorted.len();
        if start == end {
            break;
        }
    }
    roots
}

/// Everything [`set_edge_coverage`] needs to weigh a climbed pixel.
struct EdgeCoverage<'a> {
    sample: &'a [u8],
    target: &'a [u8],
    occluders: Option<&'a [u8]>,
    seed: [u8; 4],
    tolerance: u8,
    paint: FillPaint,
}

/// Work out how much of each climbed pixel the fill colour is entitled
/// to.
///
/// Each one is part boundary, part region: `t` is the boundary's share,
/// read off how far up the ramp the pixel sits, so the fill's share is
/// `1 - t`. The ramp is measured per walk, keyed by the root `climb_edge`
/// handed back, because the boundaries around one region need not be the
/// same colour - scaling a pale outline against a black one elsewhere
/// would read it as barely a boundary at all and paint most of it over.
/// What else varies is who is going to cover that pixel.
/// Anything sitting on top of the fill - the target layer's own line
/// art, or a layer above it - already contributes its share of the
/// blend, so the fill can go in solid underneath and let it do the
/// work. With nothing above, the fill has to carry the soft edge
/// itself, or the boundary below it shows through the gap.
///
/// [`FillPaint::Over`] is the odd one out: the pixel is a mixture of the
/// seed colour and the boundary, and the paint step swaps the seed's
/// share for the fill colour, so the share is all it needs.
fn set_edge_coverage(ctx: EdgeCoverage, band: &[u32], roots: &[u32], coverage: &mut [u8]) {
    let root_of = |i: usize, idx: u32| roots.get(i).copied().unwrap_or(idx);
    let mut crests: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for (i, &idx) in band.iter().enumerate() {
        let d = pixel_diff(ctx.sample, idx as usize, ctx.seed);
        crests
            .entry(root_of(i, idx))
            .and_modify(|c| *c = (*c).max(d))
            .or_insert(d);
    }
    let over = matches!(ctx.paint, FillPaint::Over { .. });

    for (i, &idx) in band.iter().enumerate() {
        let crest = crests.get(&root_of(i, idx)).copied().unwrap_or(0);
        let floor = u32::from(ctx.tolerance).min(crest);
        let span = crest.saturating_sub(floor).max(1);
        let idx = idx as usize;
        let d = pixel_diff(ctx.sample, idx, ctx.seed).saturating_sub(floor);
        let boundary = (d as f32 / span as f32).clamp(0.0, 1.0);
        let mut share = 1.0 - boundary;
        if !over {
            // How much of this pixel ends up on top of the fill.
            let above = ctx
                .occluders
                .map_or(0, |o| o[idx * 4 + 3])
                .max(ctx.target[idx * 4 + 3]);
            let above = f32::from(above) / 255.0;
            if above >= 1.0 {
                share = 1.0;
            } else {
                share = (share / (1.0 - above)).min(1.0);
            }
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            coverage[idx] = (share * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
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
    tolerance: u8,
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
        if pixel_diff(pixels, nu, seed) > u32::from(tolerance) {
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
    tolerance: u8,
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
        if pixel_diff(pixels, nu, seed) > u32::from(tolerance) {
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

/// Colour distance from the seed, 0..=255: the RMS difference across
/// the four BGRA channels, which keeps the same scale as the tolerance
/// slider (a tolerance of 32 admits a 32-per-channel shift).
#[inline]
fn pixel_diff(pixels: &[u8], idx: usize, seed: [u8; 4]) -> u32 {
    let off = idx * 4;
    let db = i32::from(pixels[off]) - i32::from(seed[0]);
    let dg = i32::from(pixels[off + 1]) - i32::from(seed[1]);
    let dr = i32::from(pixels[off + 2]) - i32::from(seed[2]);
    let da = i32::from(pixels[off + 3]) - i32::from(seed[3]);
    #[allow(clippy::cast_sign_loss)]
    let sum = (db * db + dg * dg + dr * dr + da * da) as u32;
    (sum / 4).isqrt()
}

/// Paint `result` into a premultiplied BGRA8 layer buffer in place.
///
/// Colour maths runs in linear light because the layer images are
/// sRGB-encoded and the GPU preview blends the same way; alpha is
/// already linear.
pub fn paint_fill(buffer: &mut [u8], result: &FillResult, color_bgr: [u8; 3]) {
    let fill_linear = [
        oxiedraw_utils::color::srgb_to_linear(color_bgr[0]),
        oxiedraw_utils::color::srgb_to_linear(color_bgr[1]),
        oxiedraw_utils::color::srgb_to_linear(color_bgr[2]),
    ];
    for &idx in &result.sorted_indices {
        let idx = idx as usize;
        let off = idx * 4;
        if off + 4 > buffer.len() {
            continue;
        }
        let weight = result.coverage.get(idx).copied().unwrap_or(255);
        if weight == 0 {
            continue;
        }
        match result.paint {
            FillPaint::Behind => paint_behind(buffer, off, fill_linear, weight),
            FillPaint::Over { seed } => {
                paint_over(buffer, off, color_bgr, fill_linear, seed, weight);
            }
        }
    }
}

/// Rounded `a * b / 255` for 8-bit channel maths.
#[allow(clippy::cast_possible_truncation)]
#[inline]
fn mul255(a: u8, b: u8) -> u8 {
    let t = u32::from(a) * u32::from(b) + 128;
    ((t + (t >> 8)) >> 8) as u8
}

/// Slide the fill under what is already there. The share was worked out
/// against the target's alpha already, so this just adds the fill's part
/// of the pixel: an opaque pixel got a share of zero and is left alone,
/// which is exactly why a line keeps its anti-aliased edge.
fn paint_behind(buffer: &mut [u8], off: usize, fill_linear: [f32; 3], weight: u8) {
    let share = f32::from(weight) / 255.0;
    for c in 0..3 {
        let dst = oxiedraw_utils::color::srgb_to_linear(buffer[off + c]);
        buffer[off + c] = oxiedraw_utils::color::linear_to_srgb(dst + fill_linear[c] * share);
    }
    buffer[off + 3] = buffer[off + 3].saturating_add(weight);
}

/// Replace the seed colour's share of the pixel with the fill colour.
///
/// At full weight that is a plain overwrite. Below it the pixel is a
/// mix of seed and boundary, so only the seed's part is swapped out -
/// `dst + (fill - seed) * weight` - which rebuilds the identical blend
/// around the new colour and leaves the boundary's contribution alone.
fn paint_over(
    buffer: &mut [u8],
    off: usize,
    color_bgr: [u8; 3],
    fill_linear: [f32; 3],
    seed: [u8; 4],
    weight: u8,
) {
    if weight == 255 {
        buffer[off] = color_bgr[0];
        buffer[off + 1] = color_bgr[1];
        buffer[off + 2] = color_bgr[2];
        buffer[off + 3] = 255;
        return;
    }
    let share = f32::from(weight) / 255.0;
    for c in 0..3 {
        let dst = oxiedraw_utils::color::srgb_to_linear(buffer[off + c]);
        let seed_linear = oxiedraw_utils::color::srgb_to_linear(seed[c]);
        buffer[off + c] =
            oxiedraw_utils::color::linear_to_srgb(dst + (fill_linear[c] - seed_linear) * share);
    }
    let delta = (255.0 - f32::from(seed[3])) * share;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let added = delta.round().clamp(0.0, 255.0) as u8;
    buffer[off + 3] = buffer[off + 3].saturating_add(added);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain binary fill with no edge pass - the behaviour the
    /// region-shape tests below are written against.
    fn hard(tolerance: u8) -> FillOptions {
        FillOptions {
            tolerance,
            auto_edge: false,
            ..FillOptions::default()
        }
    }

    /// A single-row canvas built from opaque greys.
    fn grey_row(shades: &[u8]) -> Vec<u8> {
        let mut pixels = vec![0u8; shades.len() * 4];
        for (i, &s) in shades.iter().enumerate() {
            pixels[i * 4..i * 4 + 4].copy_from_slice(&[s, s, s, 255]);
        }
        pixels
    }

    /// A single-row canvas of black at the given alphas - line art on
    /// an otherwise empty layer.
    fn black_row(alphas: &[u8]) -> Vec<u8> {
        let mut pixels = vec![0u8; alphas.len() * 4];
        for (i, &a) in alphas.iter().enumerate() {
            pixels[i * 4 + 3] = a;
        }
        pixels
    }

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
        let r = flood_fill(&FillSource::single(&pixels, None), w, h, sx, sy, hard(0))
            .expect("seed in bounds");
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
        let r = flood_fill(&FillSource::single(&pixels, None), w, h, 0, 0, hard(0)).expect("seed");
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
        let r = flood_fill(&FillSource::single(&pixels, None), w, h, 0, 0, hard(0)).expect("seed");
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
        let r = flood_fill(&FillSource::single(&pixels, Some(&mask)), w, h, 0, 0, hard(0))
            .expect("seed");
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
        let r = flood_fill(&FillSource::single(&pixels, Some(&mask)), w, h, 1, 0, hard(0))
            .expect("in bounds");
        assert!(r.sorted_indices.is_empty());
    }

    /// `paint_fill` writes premul `[B, G, R, 255]` at each listed
    /// pixel and leaves the others untouched.
    #[test]
    fn paint_fill_writes_color_at_listed_pixels() {
        let mut buf = vec![0u8; 12];
        let result = FillResult {
            sorted_indices: vec![0, 2],
            distance_mask: vec![255; 3],
            coverage: vec![255, 0, 255],
            paint: FillPaint::Over { seed: [0, 0, 0, 255] },
        };
        paint_fill(&mut buf, &result, [10, 20, 30]);
        assert_eq!(&buf[0..4], &[10, 20, 30, 255]);
        assert_eq!(&buf[4..8], &[0, 0, 0, 0]);
        assert_eq!(&buf[8..12], &[10, 20, 30, 255]);
    }

    /// The edge climb walks up an anti-aliased ramp and halts at its
    /// crest, so the fill reaches the middle of the line and never the
    /// far side - with no radius supplied.
    #[test]
    fn edge_climb_stops_at_the_ramp_crest() {
        // White, white, then a line ramping to black and back out.
        let pixels = grey_row(&[255, 255, 191, 64, 0, 64, 255]);
        let r = flood_fill(
            &FillSource::single(&pixels, None),
            7,
            1,
            0,
            0,
            FillOptions::default(),
        )
        .expect("seed");
        let filled: std::collections::HashSet<u32> = r.sorted_indices.iter().copied().collect();
        assert_eq!(
            filled,
            [0, 1, 2, 3, 4].into_iter().collect(),
            "fill reaches the line core and stops there"
        );
    }

    /// The climb is still confined by the selection mask.
    #[test]
    fn edge_climb_respects_selection_mask() {
        let pixels = grey_row(&[255, 128, 64, 0]);
        let opts = FillOptions {
            tolerance: 0,
            auto_edge: true,
            ..FillOptions::default()
        };
        let mask = vec![255u8, 255, 0, 0];
        let r =
            flood_fill(&FillSource::single(&pixels, Some(&mask)), 4, 1, 0, 0, opts).expect("seed");
        let filled: std::collections::HashSet<u32> = r.sorted_indices.iter().copied().collect();
        assert_eq!(filled, [0, 1].into_iter().collect());
    }

    /// Each boundary around a region is weighed against its own ramp.
    /// A region hemmed in by a black outline on one side and a pale one
    /// on the other used to share a single crest, which read the pale
    /// outline as barely a boundary and handed most of it to the fill.
    #[test]
    fn edge_ramps_are_measured_per_boundary() {
        // A pale line and its anti-aliasing, the region, then a black line.
        let pixels = grey_row(&[128, 192, 255, 255, 0]);
        let r = flood_fill(
            &FillSource::single(&pixels, None),
            5,
            1,
            2,
            0,
            FillOptions::default(),
        )
        .expect("seed");

        assert_eq!(r.coverage[0], 0, "the pale outline's core is left alone");
        assert_eq!(r.coverage[4], 0, "and so is the black outline's");
        let ramp = r.coverage[1];
        assert!(
            ramp > 0 && ramp < 255,
            "the pale outline's anti-aliasing is shared: {ramp}"
        );
    }

    /// Clicking empty canvas fills behind the art, so an anti-aliased
    /// line keeps its exact edge blending: every skirt pixel ends up
    /// opaque, and the darker the skirt was the less fill shows.
    #[test]
    fn behind_paint_preserves_line_antialiasing() {
        let alphas = [0u8, 0, 64, 160, 255];
        let pixels = black_row(&alphas);
        let r = flood_fill(
            &FillSource::single(&pixels, None),
            5,
            1,
            0,
            0,
            FillOptions::default(),
        )
        .expect("seed");
        assert_eq!(r.paint, FillPaint::Behind, "empty seed fills underneath");

        let mut buf = pixels.clone();
        paint_fill(&mut buf, &r, [0, 0, 255]);
        for (i, &a_before) in alphas.iter().enumerate() {
            let o = i * 4;
            assert_eq!(buf[o + 3], 255, "pixel {i} ends up opaque");
            // The line's own coverage survives untouched: what is left
            // for the fill is exactly the pixel's transparency.
            let expected_red = oxiedraw_utils::color::linear_to_srgb(
                oxiedraw_utils::color::srgb_to_linear(255) * (1.0 - f32::from(a_before) / 255.0),
            );
            assert_eq!(buf[o + 2], expected_red, "pixel {i} red channel");
            assert_eq!(buf[o], 0, "no blue leaked in");
        }
    }

    /// Skipping the reveal order drops the animation's extras and
    /// nothing else - the same pixels are filled with the same coverage,
    /// just left in BFS order.
    #[test]
    fn skipping_reveal_order_keeps_the_same_fill() {
        let pixels = grey_row(&[255, 255, 191, 64, 0]);
        let source = FillSource::single(&pixels, None);
        let swept = flood_fill(&source, 5, 1, 0, 0, FillOptions::default()).expect("seed");
        let plain = flood_fill(
            &source,
            5,
            1,
            0,
            0,
            FillOptions {
                reveal_order: false,
                ..FillOptions::default()
            },
        )
        .expect("seed");

        assert_eq!(plain.coverage, swept.coverage, "same pixels, same shares");
        assert_eq!(plain.paint, swept.paint);
        let as_set = |v: &[u32]| v.iter().copied().collect::<std::collections::HashSet<_>>();
        assert_eq!(as_set(&plain.sorted_indices), as_set(&swept.sorted_indices));
        assert!(
            plain.distance_mask.is_empty(),
            "the animation's mask isn't built"
        );
    }

    /// Sampling every layer while painting into an empty one: the
    /// region comes off the composite, but the paint mode has to follow
    /// the target, or the fill would try to un-mix pixels that aren't
    /// there and leave a gap under the line.
    ///
    /// With the line art *above* the fill layer, the edge pixel goes in
    /// solid - the line on top supplies the blending.
    #[test]
    fn edge_is_solid_when_the_outline_sits_above() {
        // Composite: white page, a half-blended pixel, then the line.
        let composite = grey_row(&[255, 255, 128, 0]);
        let target = vec![0u8; 16];
        // The line art layer that sits on top of the target.
        let above = black_row(&[0, 0, 128, 255]);
        let source = FillSource {
            sample: &composite,
            target: &target,
            occluders: Some(&above),
            mask: None,
        };
        let r = flood_fill(&source, 4, 1, 0, 0, FillOptions::default()).expect("seed");
        assert_eq!(r.paint, FillPaint::Behind, "empty target fills underneath");

        let mut buf = target.clone();
        paint_fill(&mut buf, &r, [0, 0, 255]);
        for px in 0..3 {
            let o = px * 4;
            assert_eq!(buf[o + 3], 255, "pixel {px} is solid under the line");
            assert_eq!(buf[o + 2], 255, "pixel {px} is full-strength fill");
        }
    }

    /// Same setup with nothing above the fill layer - the line art is
    /// below it. Now the fill has to carry the soft edge itself, or it
    /// would paint over the very line it is supposed to meet.
    #[test]
    fn edge_feathers_when_the_outline_sits_below() {
        let composite = grey_row(&[255, 255, 128, 0]);
        let target = vec![0u8; 16];
        let source = FillSource {
            sample: &composite,
            target: &target,
            occluders: None,
            mask: None,
        };
        let r = flood_fill(&source, 4, 1, 0, 0, FillOptions::default()).expect("seed");

        assert_eq!(r.coverage[0], 255, "the flat interior is still solid");
        let edge = r.coverage[2];
        assert!(
            edge > 0 && edge < 255,
            "the blended pixel gets a partial share: {edge}"
        );
        let mut buf = target.clone();
        paint_fill(&mut buf, &r, [0, 0, 255]);
        assert_eq!(buf[2 * 4 + 3], edge, "and lands at exactly that alpha");
    }

    /// Filling a solid colour that meets a line un-mixes the blended
    /// edge pixels: the seed's share becomes fill, the line's share
    /// stays put, so no hard staircase appears at the boundary.
    #[test]
    fn over_paint_unmixes_the_blended_edge() {
        // White page, black line, one half-blended pixel between them.
        let pixels = grey_row(&[255, 255, 128, 0]);
        let r = flood_fill(
            &FillSource::single(&pixels, None),
            4,
            1,
            0,
            0,
            FillOptions::default(),
        )
        .expect("seed");
        assert!(
            matches!(r.paint, FillPaint::Over { .. }),
            "opaque seed replaces the region"
        );
        let mut buf = pixels.clone();
        paint_fill(&mut buf, &r, [0, 0, 255]);

        assert_eq!(&buf[0..4], &[0, 0, 255, 255], "flat area is solid fill");
        let blended = &buf[8..12];
        assert!(
            blended[2] > 0 && blended[2] < 255,
            "edge pixel keeps a partial red: {blended:?}"
        );
        assert!(
            blended[0] < 128 && blended[1] < 128,
            "the white that used to show through is gone: {blended:?}"
        );
        assert_eq!(&buf[12..16], &[0, 0, 0, 255], "the line core is untouched");
    }
}
