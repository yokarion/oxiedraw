//! Pulling a palette out of an image.
//!
//! One pass builds a blurred OKLab density histogram and mode-seeks it into
//! peaks; each peak reports the most common pixel value at its densest cell, so
//! every colour that comes out is one that is actually in the picture rather
//! than an average of a region. Peaks are then chosen by share, chroma and how
//! isolated they are, which is what lets a handful of pixels of eye green beat
//! a fourth shade of the background grey. Analysis is cached in
//! [`PaletteSource`] so moving a slider only re-runs the selection.

use std::collections::HashMap;

use crate::color::Color;
use crate::enum_meta::EnumMeta;
use oxiedraw_utils::color as color_math;

// The histogram grid. 64 bins keep the shades of one flat colour in a single
// cell, and a/b stay inside +/-0.35 for the whole sRGB gamut.
const BINS: usize = 64;
const CELLS: usize = BINS * BINS * BINS;
const AB_LIMIT: f32 = 0.35;

const SAMPLE_BUDGET: usize = 4_194_304;
const ALPHA_FLOOR: u8 = 24;
const MAX_CANDIDATES: usize = 512;

// Ranking. Tuned against a set of reference palettes, not derived - a weight
// exponent well under 1 is what keeps a tenth-of-a-percent accent competing
// with the background, and the rarity bonus measures hue and chroma only, so
// separation stays the thing that spaces a palette out by lightness.
const WEIGHT_EXPONENT: f32 = 0.30;
const CHROMA_BONUS: f32 = 2.5;
const RARITY_BONUS: f32 = 4.0;
const RARITY_DOMINANCE: f32 = 4.0;
const MIN_SEPARATION: f32 = 0.015;
const DETAIL_MIDPOINT: f32 = 0.10;
const DETAIL_SPAN: f32 = 3.0;

/// Enough for one backdrop entry to survive, not for every tier of a gradient.
const SMOOTH_BACKGROUND_WEIGHT: f32 = 0.12;

// Background flood. Past `BAIL_OUT` the flood has escaped into the drawing and
// nothing is called background, except when what it covers is all one colour
// (`FLAT_SPREAD`), which is just a sketch on blank paper.
const BACKGROUND_PROBE: usize = 256;
const BACKGROUND_TOLERANCE: f32 = 0.035;
const BACKGROUND_SEED_TOLERANCE: f32 = 0.10;
const BACKGROUND_MATCH_TOLERANCE: f32 = 0.06;
const BACKGROUND_BAIL_OUT: f32 = 0.70;
const BACKGROUND_FLAT_SPREAD: f32 = 0.02;
const BACKGROUND_FLAT_BAIL_OUT: f32 = 0.97;

pub const MIN_EXTRACTED_COLORS: usize = 2;
pub const MAX_EXTRACTED_COLORS: usize = 24;

/// How the extracted colours are laid out in the finished palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractOrder {
    Frequency,
    Hue,
    Lightness,
}

impl EnumMeta for ExtractOrder {
    const ALL: &'static [Self] = &[Self::Frequency, Self::Hue, Self::Lightness];

    fn label(self) -> &'static str {
        match self {
            Self::Frequency => "Frequency",
            Self::Hue => "Hue",
            Self::Lightness => "Lightness",
        }
    }
}

/// How much of the backdrop behind the drawing belongs in the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundMode {
    Ignore,
    Smooth,
    Detailed,
}

impl EnumMeta for BackgroundMode {
    const ALL: &'static [Self] = &[Self::Ignore, Self::Smooth, Self::Detailed];

    fn label(self) -> &'static str {
        match self {
            Self::Ignore => "Ignore",
            Self::Smooth => "Smooth",
            Self::Detailed => "Detailed",
        }
    }
}

impl BackgroundMode {
    const fn weight(self) -> f32 {
        match self {
            Self::Ignore => 0.0,
            Self::Smooth => SMOOTH_BACKGROUND_WEIGHT,
            Self::Detailed => 1.0,
        }
    }
}

/// Knobs the Extract Palette window exposes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtractOptions {
    /// 0 keeps only what the picture is mostly made of, 1 chases every accent.
    /// The colour count follows from it rather than being asked for outright.
    pub detail: f32,
    /// Hard ceiling on the result, whatever the detail slider wants.
    pub limit: usize,
    pub background: BackgroundMode,
    pub order: ExtractOrder,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            detail: 0.5,
            limit: MAX_EXTRACTED_COLORS,
            background: BackgroundMode::Ignore,
            order: ExtractOrder::Frequency,
        }
    }
}

/// One colour of the result, with the share of sampled pixels behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtractedColor {
    pub color: Color,
    /// Fraction of the surviving samples, summing to 1 across the result.
    pub weight: f32,
}

/// A density peak: one colour the picture is built from.
#[derive(Clone, Copy)]
struct Candidate {
    color: Color,
    lab: [f32; 3],
    chroma: f32,
    foreground: f32,
    background: f32,
}

/// The expensive half of extraction, kept so the knobs stay live: building this
/// reads the pixels, [`PaletteSource::select`] answers from the peaks alone.
pub struct PaletteSource {
    candidates: Vec<Candidate>,
    total: f32,
}

impl PaletteSource {
    /// Analyse premultiplied BGRA8, one R8 `mask` byte per pixel. A mask skips
    /// background detection: the selection already said where to look.
    #[must_use]
    pub fn analyze(bgra: &[u8], width: u32, mask: Option<&[u8]>) -> Self {
        let pixels = bgra.len() / 4;
        let width = width as usize;
        if pixels == 0 || width == 0 {
            return Self::empty();
        }
        let height = pixels / width;
        let covered = mask.map_or(pixels, |m| m.iter().filter(|byte| **byte != 0).count());
        if covered == 0 {
            return Self::empty();
        }
        // A square grid, not every Nth pixel of the flat buffer: a stride that
        // divides the width lands on the same few columns forever.
        let step = ((covered.div_ceil(SAMPLE_BUDGET) as f64).sqrt().ceil() as usize).max(1);

        let table = linear_table();
        let backdrop = match mask {
            Some(_) => None,
            None => detect_background(&table, bgra, width, height),
        };

        let mut foreground = vec![0.0_f32; CELLS];
        let mut background = vec![0.0_f32; CELLS];
        // Packed as (cell, rgb) rather than wider types: at a 4k canvas this is
        // millions of entries and the pair has to stay eight bytes.
        let mut samples: Vec<(u32, u32)> = Vec::with_capacity(covered.div_ceil(step * step));
        let mut total = 0.0_f32;

        for y in (0..height).step_by(step) {
            for x in (0..width).step_by(step) {
                let index = y * width + x;
                if mask.is_some_and(|m| m.get(index).copied().unwrap_or(0) == 0) {
                    continue;
                }
                let p = &bgra[index * 4..index * 4 + 4];
                let alpha = p[3];
                if alpha < ALPHA_FLOOR {
                    continue;
                }
                let color = Color::new(
                    straighten(p[2], alpha),
                    straighten(p[1], alpha),
                    straighten(p[0], alpha),
                );
                let lab = oklab_with(&table, color);
                let cell = cell_of_lab(lab);
                if backdrop
                    .as_ref()
                    .is_some_and(|b| b.contains(x, y, width, height, lab))
                {
                    background[cell] += 1.0;
                } else {
                    foreground[cell] += 1.0;
                }
                samples.push((cell as u32, packed(color)));
                total += 1.0;
            }
        }
        if total == 0.0 {
            return Self::empty();
        }

        let mut density: Vec<f32> = foreground
            .iter()
            .zip(&background)
            .map(|(f, b)| f + b)
            .collect();
        blur(&mut density);
        let basin_of = ascend(&density);

        Self {
            candidates: collect(&basin_of, &foreground, &background, &samples),
            total,
        }
    }

    fn empty() -> Self {
        Self {
            candidates: Vec::new(),
            total: 0.0,
        }
    }

    /// Choose a palette from the peaks. Cheap: no pixels are touched.
    #[must_use]
    pub fn select(&self, options: &ExtractOptions) -> Vec<ExtractedColor> {
        if self.candidates.is_empty() || self.total <= 0.0 {
            return Vec::new();
        }
        let backdrop = options.background.weight();
        let weights: Vec<f32> = self
            .candidates
            .iter()
            .map(|c| (c.foreground + backdrop * c.background) / self.total)
            .collect();

        let scores = rank(&self.candidates, &weights);
        let limit = options.limit.clamp(MIN_EXTRACTED_COLORS, MAX_EXTRACTED_COLORS);
        let picked = choose(&self.candidates, &weights, &scores, options.detail, limit);

        let total: f32 = picked.iter().map(|i| weights[*i]).sum();
        let mut out: Vec<ExtractedColor> = picked
            .into_iter()
            .map(|i| ExtractedColor {
                color: self.candidates[i].color,
                weight: if total > 0.0 { weights[i] / total } else { 0.0 },
            })
            .collect();
        order(&mut out, options.order);
        out
    }
}

/// Extract a palette in one call. The Extract window holds a [`PaletteSource`]
/// instead so its sliders do not re-read the canvas.
#[must_use]
pub fn extract_palette(
    bgra: &[u8],
    width: u32,
    mask: Option<&[u8]>,
    options: &ExtractOptions,
) -> Vec<ExtractedColor> {
    PaletteSource::analyze(bgra, width, mask).select(options)
}

/// Un-premultiply one channel back to its straight value.
fn straighten(channel: u8, alpha: u8) -> u8 {
    if alpha == 0 || alpha == 255 {
        return channel;
    }
    // The cast saturates, which is the clamp for a channel above its alpha.
    (f32::from(channel) * 255.0 / f32::from(alpha)) as u8
}

/// sRGB byte to linear, tabulated: the transfer function's `powf` is otherwise
/// the most expensive thing in a pass that runs over every pixel.
fn linear_table() -> [f32; 256] {
    std::array::from_fn(|c| color_math::srgb_to_linear(c as u8))
}

fn oklab_with(table: &[f32; 256], color: Color) -> [f32; 3] {
    color_math::linear_rgb_to_oklab([
        table[color.r as usize],
        table[color.g as usize],
        table[color.b as usize],
    ])
}

fn cell_of_lab(lab: [f32; 3]) -> usize {
    let axis = |v: f32, lo: f32, hi: f32| {
        let t = ((v - lo) / (hi - lo) * BINS as f32) as isize;
        t.clamp(0, BINS as isize - 1) as usize
    };
    let l = axis(lab[0], 0.0, 1.0);
    let a = axis(lab[1], -AB_LIMIT, AB_LIMIT);
    let b = axis(lab[2], -AB_LIMIT, AB_LIMIT);
    (l * BINS + a) * BINS + b
}

/// Separable [1, 2, 1] over the grid, so the spread of an anti-aliased edge
/// gathers into one peak instead of a row of ties.
fn blur(volume: &mut [f32]) {
    let mut line = vec![0.0_f32; BINS];
    for axis in 0..3 {
        let (outer, stride) = match axis {
            0 => (BINS * BINS, BINS * BINS),
            1 => (BINS * BINS, BINS),
            _ => (BINS * BINS, 1),
        };
        for block in 0..outer {
            let start = match axis {
                0 => block,
                1 => (block / BINS) * BINS * BINS + block % BINS,
                _ => block * BINS,
            };
            for i in 0..BINS {
                line[i] = volume[start + i * stride];
            }
            for i in 0..BINS {
                let prev = line[i.saturating_sub(1)];
                let next = line[(i + 1).min(BINS - 1)];
                volume[start + i * stride] = prev.mul_add(0.25, next.mul_add(0.25, line[i] * 0.5));
            }
        }
    }
}

/// Steepest ascent to a local maximum, path-compressed so every cell names the
/// peak it belongs to. Empty cells are skipped and stay pointing at themselves.
fn ascend(density: &[f32]) -> Vec<u32> {
    let mut up: Vec<u32> = (0..CELLS as u32).collect();
    let live: Vec<u32> = (0..CELLS as u32).filter(|c| density[*c as usize] > 0.0).collect();

    for &cell in &live {
        let here = cell as usize;
        let (l, a, b) = (here / (BINS * BINS), (here / BINS) % BINS, here % BINS);
        let mut best = density[here];
        let mut best_at = here;
        for dl in -1_isize..=1 {
            for da in -1_isize..=1 {
                for db in -1_isize..=1 {
                    let (nl, na, nb) = (l as isize + dl, a as isize + da, b as isize + db);
                    if nl < 0 || na < 0 || nb < 0 {
                        continue;
                    }
                    let (nl, na, nb) = (nl as usize, na as usize, nb as usize);
                    if nl >= BINS || na >= BINS || nb >= BINS {
                        continue;
                    }
                    let there = (nl * BINS + na) * BINS + nb;
                    // Bit-equal densities go to the lower cell, so a blurred
                    // plateau becomes one basin instead of one per cell. The
                    // exact compare is the point: near-ties must not merge.
                    #[allow(clippy::float_cmp)]
                    let tied = density[there] == best;
                    if density[there] > best || (tied && there < best_at) {
                        best = density[there];
                        best_at = there;
                    }
                }
            }
        }
        up[here] = best_at as u32;
    }

    for _ in 0..12 {
        for &cell in &live {
            up[cell as usize] = up[up[cell as usize] as usize];
        }
    }
    up
}

/// Turn basins into candidates: the densest raw cell of each basin speaks for
/// it, and the most common exact pixel value there is the colour reported.
fn collect(
    basin_of: &[u32],
    foreground: &[f32],
    background: &[f32],
    samples: &[(u32, u32)],
) -> Vec<Candidate> {
    let mut index_of: HashMap<u32, usize> = HashMap::new();
    let mut peak_cell: Vec<usize> = Vec::new();
    let mut peak_density: Vec<f32> = Vec::new();
    let mut foreground_sum: Vec<f32> = Vec::new();
    let mut background_sum: Vec<f32> = Vec::new();

    for cell in 0..CELLS {
        let raw = foreground[cell] + background[cell];
        if raw <= 0.0 {
            continue;
        }
        let basin = basin_of[cell];
        let slot = *index_of.entry(basin).or_insert_with(|| {
            peak_cell.push(cell);
            peak_density.push(0.0);
            foreground_sum.push(0.0);
            background_sum.push(0.0);
            peak_cell.len() - 1
        });
        foreground_sum[slot] += foreground[cell];
        background_sum[slot] += background[cell];
        if raw > peak_density[slot] {
            peak_density[slot] = raw;
            peak_cell[slot] = cell;
        }
    }

    // Cell to slot as a flat table, not a hash lookup: this runs once per
    // sampled pixel, and on flat art nearly every sample reaches it.
    let mut slot_of_peak = vec![u32::MAX; CELLS];
    for (slot, cell) in peak_cell.iter().enumerate() {
        slot_of_peak[*cell] = slot as u32;
    }
    let mut tally: HashMap<(usize, u32), u32> = HashMap::new();
    for &(cell, color) in samples {
        let slot = slot_of_peak[cell as usize];
        if slot != u32::MAX {
            *tally.entry((slot as usize, color)).or_insert(0) += 1;
        }
    }
    // Ties broken by colour value, not by hash order: two shades with the same
    // count must not give a different palette on every run.
    let mut best: Vec<(u32, u32)> = vec![(0, 0); peak_cell.len()];
    for ((slot, color), count) in tally {
        let (kept, top) = best[slot];
        if count > top || (count == top && color < kept) {
            best[slot] = (color, count);
        }
    }

    let mut out: Vec<Candidate> = (0..peak_cell.len())
        .filter(|slot| best[*slot].1 > 0)
        .map(|slot| {
            let color = unpacked(best[slot].0);
            let lab = color_math::rgb_to_oklab(color.r, color.g, color.b);
            Candidate {
                color,
                lab,
                chroma: lab[1].hypot(lab[2]),
                foreground: foreground_sum[slot],
                background: background_sum[slot],
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (b.foreground + b.background)
            .total_cmp(&(a.foreground + a.background))
            .then_with(|| packed(a.color).cmp(&packed(b.color)))
    });
    out.truncate(MAX_CANDIDATES);
    out
}

const fn packed(color: Color) -> u32 {
    ((color.r as u32) << 16) | ((color.g as u32) << 8) | color.b as u32
}

const fn unpacked(value: u32) -> Color {
    Color::new(
        ((value >> 16) & 255) as u8,
        ((value >> 8) & 255) as u8,
        (value & 255) as u8,
    )
}

/// Standing of each peak before anything is chosen: mostly its share, lifted by
/// its own chroma and by sitting far from every much more common colour.
fn rank(candidates: &[Candidate], weights: &[f32]) -> Vec<f32> {
    candidates
        .iter()
        .enumerate()
        .map(|(i, candidate)| {
            if weights[i] <= 0.0 {
                return 0.0;
            }
            let rarity = candidates
                .iter()
                .enumerate()
                .filter(|(j, _)| weights[*j] >= RARITY_DOMINANCE * weights[i])
                .map(|(_, other)| {
                    (candidate.lab[1] - other.lab[1]).hypot(candidate.lab[2] - other.lab[2])
                })
                .min_by(f32::total_cmp)
                .unwrap_or(0.0);
            weights[i].powf(WEIGHT_EXPONENT)
                * CHROMA_BONUS.mul_add(candidate.chroma, 1.0)
                * RARITY_BONUS.mul_add(rarity, 1.0)
        })
        .collect()
}

/// Take the strongest peak, then keep taking whichever adds most that is not
/// already covered, until the next one adds too little to be worth a swatch.
fn choose(
    candidates: &[Candidate],
    weights: &[f32],
    scores: &[f32],
    detail: f32,
    limit: usize,
) -> Vec<usize> {
    let Some(first) = (0..candidates.len())
        .filter(|i| weights[*i] > 0.0)
        .max_by(|a, b| scores[*a].total_cmp(&scores[*b]))
    else {
        return Vec::new();
    };
    let mut picked = vec![first];
    // Relative to the second pick: the absolute scale of the gain swings by an
    // order of magnitude between flat art and a painting, the shape does not.
    let bar = DETAIL_MIDPOINT * DETAIL_SPAN.powf(-detail.clamp(0.0, 1.0).mul_add(2.0, -1.0));
    let mut reference = 0.0_f32;

    while picked.len() < limit {
        let mut best = None;
        for (i, candidate) in candidates.iter().enumerate() {
            if weights[i] <= 0.0 || picked.contains(&i) {
                continue;
            }
            let separation = picked
                .iter()
                .map(|p| color_math::oklab_distance(candidate.lab, candidates[*p].lab))
                .min_by(f32::total_cmp)
                .unwrap_or(0.0);
            if separation < MIN_SEPARATION {
                continue;
            }
            let gain = scores[i] * separation;
            if best.is_none_or(|(_, top)| gain > top) {
                best = Some((i, gain));
            }
        }
        let Some((i, gain)) = best else { break };
        if reference <= 0.0 {
            reference = gain;
        }
        if reference <= 0.0 || gain / reference <= bar {
            break;
        }
        picked.push(i);
    }
    picked
}

fn order(colors: &mut [ExtractedColor], order: ExtractOrder) {
    match order {
        ExtractOrder::Frequency => colors.sort_by(|a, b| b.weight.total_cmp(&a.weight)),
        ExtractOrder::Hue => {
            colors.sort_by(|a, b| super::hue_rank(a.color).total_cmp(&super::hue_rank(b.color)));
        }
        ExtractOrder::Lightness => {
            colors.sort_by(|a, b| super::lightness(a.color).total_cmp(&super::lightness(b.color)));
        }
    }
}

/// The backdrop behind the drawing, as a mask over a downscaled copy.
struct Backdrop {
    seen: Vec<bool>,
    lab: Vec<[f32; 3]>,
    width: usize,
    height: usize,
}

impl Backdrop {
    /// Backdrop when the pixel's own colour matches a backdrop probe beside it:
    /// on colour so an accent inside the backdrop survives, on the neighbours
    /// so the rim sharing a block with the drawing is not left behind.
    fn contains(&self, x: usize, y: usize, width: usize, height: usize, lab: [f32; 3]) -> bool {
        let sx = (x * self.width / width.max(1)).min(self.width - 1) as isize;
        let sy = (y * self.height / height.max(1)).min(self.height - 1) as isize;
        (-1..=1).any(|dy| {
            (-1..=1).any(|dx| {
                let (nx, ny) = (sx + dx, sy + dy);
                if nx < 0 || ny < 0 || nx >= self.width as isize || ny >= self.height as isize {
                    return false;
                }
                let at = ny as usize * self.width + nx as usize;
                self.seen[at]
                    && color_math::oklab_distance(self.lab[at], lab) <= BACKGROUND_MATCH_TOLERANCE
            })
        })
    }
}

/// Flood inward from the canvas edge, letting neighbouring pixels drift a
/// little so a gradient backdrop is followed but a drawn edge stops it.
fn detect_background(
    table: &[f32; 256],
    bgra: &[u8],
    width: usize,
    height: usize,
) -> Option<Backdrop> {
    if width < 2 || height < 2 {
        return None;
    }
    let step = (width.max(height).div_ceil(BACKGROUND_PROBE)).max(1);
    let (sw, sh) = (width.div_ceil(step), height.div_ceil(step));
    if sw < 2 || sh < 2 {
        return None;
    }

    let mut lab = vec![[0.0_f32; 3]; sw * sh];
    let mut opaque = vec![false; sw * sh];
    for sy in 0..sh {
        for sx in 0..sw {
            let index = (sy * step).min(height - 1) * width + (sx * step).min(width - 1);
            let p = &bgra[index * 4..index * 4 + 4];
            let alpha = p[3];
            opaque[sy * sw + sx] = alpha >= ALPHA_FLOOR;
            lab[sy * sw + sx] = oklab_with(
                table,
                Color::new(
                    straighten(p[2], alpha),
                    straighten(p[1], alpha),
                    straighten(p[0], alpha),
                ),
            );
        }
    }

    let border: Vec<usize> = (0..sw)
        .flat_map(|sx| [sx, (sh - 1) * sw + sx])
        .chain((0..sh).flat_map(|sy| [sy * sw, sy * sw + sw - 1]))
        .collect();
    let dominant = dominant_color(&lab, &opaque, &border)?;

    // Seeded only where the edge agrees with the commonest edge colour: a
    // figure running off the side of the canvas is drawing, not backdrop.
    let mut seen = vec![false; sw * sh];
    let mut queue: Vec<usize> = Vec::new();
    for at in border {
        let seeds = !opaque[at]
            || color_math::oklab_distance(lab[at], dominant) <= BACKGROUND_SEED_TOLERANCE;
        if seeds && !seen[at] {
            seen[at] = true;
            queue.push(at);
        }
    }

    let mut head = 0;
    while head < queue.len() {
        let at = queue[head];
        head += 1;
        let (x, y) = (at % sw, at / sw);
        let here = lab[at];
        for (dx, dy) in [(1_isize, 0_isize), (-1, 0), (0, 1), (0, -1)] {
            let (nx, ny) = (x as isize + dx, y as isize + dy);
            if nx < 0 || ny < 0 || nx >= sw as isize || ny >= sh as isize {
                continue;
            }
            let next = ny as usize * sw + nx as usize;
            if seen[next] {
                continue;
            }
            // Transparent pixels are already excluded from the histogram, so
            // walking through them only helps the flood reach around a figure.
            let same = !opaque[next]
                || color_math::oklab_distance(lab[next], here) <= BACKGROUND_TOLERANCE;
            if same {
                seen[next] = true;
                queue.push(next);
            }
        }
    }

    let flooded: Vec<usize> = (0..sw * sh).filter(|at| seen[*at] && opaque[*at]).collect();
    let share = seen.iter().filter(|s| **s).count() as f32 / (sw * sh) as f32;
    // A big flood is usually one that escaped into the drawing, except when
    // everything it covers is the same colour, which is just a large backdrop.
    let ceiling = if spread(&lab, &flooded) <= BACKGROUND_FLAT_SPREAD {
        BACKGROUND_FLAT_BAIL_OUT
    } else {
        BACKGROUND_BAIL_OUT
    };
    if share > ceiling {
        return None;
    }
    Some(Backdrop {
        seen,
        lab,
        width: sw,
        height: sh,
    })
}

/// The commonest colour among `of`, as the mean of whichever histogram cell
/// holds the most of them.
fn dominant_color(lab: &[[f32; 3]], opaque: &[bool], of: &[usize]) -> Option<[f32; 3]> {
    let mut tally: HashMap<usize, (u32, [f32; 3])> = HashMap::new();
    for &at in of {
        if !opaque[at] {
            continue;
        }
        let entry = tally.entry(cell_of_lab(lab[at])).or_insert((0, [0.0; 3]));
        entry.0 += 1;
        for (sum, axis) in entry.1.iter_mut().zip(lab[at]) {
            *sum += axis;
        }
    }
    // Ties by cell, not by hash order, so detection stays repeatable.
    let (_, count, sum) = tally
        .into_iter()
        .map(|(cell, (count, sum))| (cell, count, sum))
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))?;
    let n = count as f32;
    Some([sum[0] / n, sum[1] / n, sum[2] / n])
}

/// Mean distance from the average colour: near zero for one flat tone, larger
/// once a flood has wandered across a drawing.
fn spread(lab: &[[f32; 3]], of: &[usize]) -> f32 {
    if of.is_empty() {
        return 0.0;
    }
    let n = of.len() as f32;
    let mut mean = [0.0_f32; 3];
    for &at in of {
        for (sum, axis) in mean.iter_mut().zip(lab[at]) {
            *sum += axis / n;
        }
    }
    of.iter()
        .map(|at| color_math::oklab_distance(lab[*at], mean))
        .sum::<f32>()
        / n
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn image(colors: &[(Color, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (color, count) in colors {
            for _ in 0..*count {
                out.extend_from_slice(&[color.b, color.g, color.r, 255]);
            }
        }
        out
    }

    fn width_of(pixels: &[u8]) -> u32 {
        (pixels.len() / 4) as u32
    }

    fn extract(pixels: &[u8], options: &ExtractOptions) -> Vec<Color> {
        extract_palette(pixels, width_of(pixels), None, options)
            .into_iter()
            .map(|c| c.color)
            .collect()
    }

    fn detailed(detail: f32) -> ExtractOptions {
        ExtractOptions {
            detail,
            background: BackgroundMode::Detailed,
            ..ExtractOptions::default()
        }
    }

    #[test]
    fn the_loudest_colors_come_out_in_frequency_order() {
        let red = Color::new(220, 40, 40);
        let blue = Color::new(40, 60, 220);
        let green = Color::new(40, 200, 80);
        let pixels = image(&[(red, 50), (blue, 30), (green, 10)]);

        let out = extract(&pixels, &detailed(1.0));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], red);
        assert_eq!(out[1], blue);
        assert_eq!(out[2], green);
    }

    #[test]
    fn weights_sum_to_one() {
        let pixels = image(&[
            (Color::new(220, 40, 40), 50),
            (Color::new(40, 60, 220), 30),
            (Color::new(40, 200, 80), 10),
        ]);
        let out = extract_palette(&pixels, width_of(&pixels), None, &detailed(1.0));
        assert!((out.iter().map(|c| c.weight).sum::<f32>() - 1.0).abs() < 1e-4);
    }

    #[test]
    fn every_extracted_color_occurs_in_the_image() {
        let tones: Vec<(Color, usize)> = (0..40)
            .map(|step| (Color::new(20 + step * 5, 90, 200 - step * 4), 7))
            .collect();
        let pixels = image(&tones);

        for color in extract(&pixels, &detailed(1.0)) {
            assert!(
                tones.iter().any(|(tone, _)| *tone == color),
                "{color:?} is an average, not a colour in the picture"
            );
        }
    }

    #[test]
    fn neutrals_are_kept() {
        let pixels = image(&[
            (Color::WHITE, 40),
            (Color::new(170, 170, 170), 30),
            (Color::new(45, 45, 45), 20),
            (Color::new(200, 60, 60), 10),
        ]);

        let out = extract(&pixels, &detailed(1.0));
        assert!(out.contains(&Color::WHITE), "white survives: {out:?}");
        assert!(out.contains(&Color::new(170, 170, 170)), "grey survives: {out:?}");
        assert!(out.contains(&Color::new(45, 45, 45)), "dark grey survives: {out:?}");
    }

    #[test]
    fn a_rare_saturated_accent_beats_another_shade_of_the_bulk() {
        let mut bands: Vec<(Color, usize)> = (0..12)
            .map(|step| (Color::new(120 + step * 6, 122 + step * 6, 128 + step * 6), 4000))
            .collect();
        let accent = Color::new(40, 230, 60);
        bands.push((accent, 12));
        let pixels = image(&bands);

        let out = extract(
            &pixels,
            &ExtractOptions {
                limit: 4,
                ..detailed(1.0)
            },
        );
        assert!(out.contains(&accent), "the accent is in the palette: {out:?}");
    }

    #[test]
    fn transparent_pixels_never_count() {
        let mut pixels = image(&[(Color::new(180, 90, 40), 20)]);
        pixels.extend_from_slice(&[10, 200, 10, 0]);

        let out = extract(&pixels, &detailed(1.0));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], Color::new(180, 90, 40));
    }

    #[test]
    fn premultiplied_pixels_come_back_at_their_straight_color() {
        let pixels = vec![64, 64, 64, 128];
        let out = extract_palette(&pixels, 1, None, &detailed(1.0));
        assert_eq!(out.len(), 1);
        assert!(out[0].color.r > 120, "un-premultiplied: {:?}", out[0].color);
    }

    #[test]
    fn a_mask_limits_sampling_to_the_selection() {
        let inside = Color::new(30, 120, 200);
        let outside = Color::new(200, 120, 30);
        let pixels = image(&[(inside, 4), (outside, 4)]);
        let mask = vec![255, 255, 255, 255, 0, 0, 0, 0];

        let out = extract_palette(&pixels, 8, Some(&mask), &detailed(1.0));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].color, inside);
    }

    #[test]
    fn a_narrow_selection_on_a_wide_canvas_is_still_found() {
        let width = 4096_usize;
        let height = 4096_usize;
        let ink = Color::new(30, 120, 200);
        let mut pixels = vec![0_u8; width * height * 4];
        let mut mask = vec![0_u8; width * height];
        for y in 0..height {
            for x in 65..115 {
                let index = y * width + x;
                pixels[index * 4..index * 4 + 4].copy_from_slice(&[ink.b, ink.g, ink.r, 255]);
                mask[index] = 255;
            }
        }

        let out = extract_palette(&pixels, width as u32, Some(&mask), &detailed(1.0));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].color, ink);
    }

    #[test]
    fn an_empty_or_fully_masked_image_yields_nothing() {
        assert!(extract_palette(&[], 0, None, &ExtractOptions::default()).is_empty());
        let pixels = image(&[(Color::new(120, 60, 30), 4)]);
        let mask = vec![0, 0, 0, 0];
        assert!(extract_palette(&pixels, 4, Some(&mask), &ExtractOptions::default()).is_empty());
    }

    #[test]
    fn more_detail_never_gives_fewer_colors() {
        let bands: Vec<(Color, usize)> = (0..24)
            .map(|step| (Color::new(20 + step * 9, 200 - step * 7, 60 + step * 5), 30))
            .collect();
        let pixels = image(&bands);

        let mut previous = 0;
        for detail in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let count = extract(&pixels, &detailed(detail)).len();
            assert!(count >= previous, "detail {detail} gave {count} after {previous}");
            previous = count;
        }
    }

    #[test]
    fn lightness_order_runs_dark_to_light() {
        let pixels = image(&[
            (Color::new(230, 200, 120), 10),
            (Color::new(60, 40, 20), 10),
            (Color::new(140, 110, 60), 10),
        ]);
        let out = extract_palette(&pixels, width_of(&pixels), None, &ExtractOptions {
            order: ExtractOrder::Lightness,
            ..detailed(1.0)
        });
        assert_eq!(out.len(), 3);
        for pair in out.windows(2) {
            assert!(super::super::lightness(pair[0].color) <= super::super::lightness(pair[1].color));
        }
    }

    /// Paint `subject` over a flat `paper` canvas, in the given pixel rect.
    fn drawing(size: usize, paper: Color, subject: Color, rect: (usize, usize, usize, usize)) -> Vec<u8> {
        let (rx, ry, rw, rh) = rect;
        let mut pixels = Vec::with_capacity(size * size * 4);
        for y in 0..size {
            for x in 0..size {
                let inside = (rx..rx + rw).contains(&x) && (ry..ry + rh).contains(&y);
                let c = if inside { subject } else { paper };
                pixels.extend_from_slice(&[c.b, c.g, c.r, 255]);
            }
        }
        pixels
    }

    fn palette_of(pixels: &[u8], size: usize, background: BackgroundMode) -> Vec<Color> {
        extract_palette(pixels, size as u32, None, &ExtractOptions {
            background,
            detail: 1.0,
            ..ExtractOptions::default()
        })
        .into_iter()
        .map(|c| c.color)
        .collect()
    }

    #[test]
    fn a_small_drawing_on_a_big_blank_canvas_still_loses_the_paper() {
        let paper = Color::new(245, 243, 238);
        let subject = Color::new(180, 70, 55);
        let size = 400;
        // Roughly a fifth of the canvas, which is what a sketch on blank paper
        // looks like and used to trip the escaped-flood bail-out.
        let pixels = drawing(size, paper, subject, (140, 140, 179, 179));

        let ignored = palette_of(&pixels, size, BackgroundMode::Ignore);
        assert!(ignored.contains(&subject), "the drawing stays: {ignored:?}");
        assert!(!ignored.contains(&paper), "the paper goes: {ignored:?}");
        assert!(palette_of(&pixels, size, BackgroundMode::Detailed).contains(&paper));
    }

    #[test]
    fn a_subject_running_off_the_canvas_edge_is_not_backdrop() {
        let paper = Color::new(245, 243, 238);
        let subject = Color::new(70, 90, 190);
        let size = 400;
        // Touches the top edge, so a flood seeded blindly from the whole border
        // would walk straight into it and delete it.
        let pixels = drawing(size, paper, subject, (150, 0, 100, 260));

        let ignored = palette_of(&pixels, size, BackgroundMode::Ignore);
        assert!(ignored.contains(&subject), "the drawing stays: {ignored:?}");
        assert!(!ignored.contains(&paper), "the paper goes: {ignored:?}");
    }

    #[test]
    fn a_pixel_unlike_its_probe_is_not_backdrop() {
        let backdrop = Color::new(60, 62, 66);
        let probe = color_math::rgb_to_oklab(backdrop.r, backdrop.g, backdrop.b);
        let mask = Backdrop {
            seen: vec![true; 4],
            lab: vec![probe; 4],
            width: 2,
            height: 2,
        };

        assert!(mask.contains(0, 0, 64, 64, probe), "the backdrop itself");
        let accent = color_math::rgb_to_oklab(25, 250, 15);
        assert!(
            !mask.contains(0, 0, 64, 64, accent),
            "an accent inside a backdrop block survives its probe"
        );
    }

    #[test]
    fn the_same_pixels_always_give_the_same_palette() {
        let tones: Vec<(Color, usize)> = (0..40)
            .map(|step| (Color::new(20 + step * 5, 90 + step, 200 - step * 4), 7))
            .collect();
        let pixels = image(&tones);

        let first = extract(&pixels, &detailed(1.0));
        for run in 1..12 {
            assert_eq!(extract(&pixels, &detailed(1.0)), first, "run {run} differed");
        }
    }

    #[test]
    fn ignoring_the_background_drops_the_flat_backdrop() {
        let (width, height) = (96_usize, 96_usize);
        let backdrop = Color::new(60, 62, 66);
        let subject = Color::new(210, 120, 40);
        let mut pixels = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let inside = (16..80).contains(&x) && (16..80).contains(&y);
                let c = if inside { subject } else { backdrop };
                pixels.extend_from_slice(&[c.b, c.g, c.r, 255]);
            }
        }

        let kept = |mode| {
            extract_palette(&pixels, width as u32, None, &ExtractOptions {
                background: mode,
                detail: 1.0,
                ..ExtractOptions::default()
            })
            .into_iter()
            .map(|c| c.color)
            .collect::<Vec<_>>()
        };

        let ignored = kept(BackgroundMode::Ignore);
        assert!(ignored.contains(&subject), "the subject stays: {ignored:?}");
        assert!(!ignored.contains(&backdrop), "the backdrop goes: {ignored:?}");
        assert!(kept(BackgroundMode::Detailed).contains(&backdrop));
    }
}
