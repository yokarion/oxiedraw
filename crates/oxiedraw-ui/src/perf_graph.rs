//! On-canvas performance overlay (toggle with F3 via the `perf-graph` action).
//!
//! Two charts share one x axis of the last [`HISTORY`] displayed frames: a total
//! frame-latency trace, and a 100% stacked breakdown of where that latency went.
//! The breakdown segments come from [`frame_profile`] spans placed through the
//! input, stamp, composite, present and snapshot path; whatever the spans did
//! not account for lands in the `Other` residual, so the stack always sums to
//! the measured frame interval. A sample is pushed once per canvas redraw, so
//! idle time between renders is ignored. System stats are read from /proc and
//! the DRM sysfs nodes and refreshed at most a couple of times a second.

use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use oxiedraw_utils::frame_profile::{self, FrameSample, STAGE_COUNT, Stage};
use relm4::gtk;

/// Number of frame samples kept (graph width, in samples).
const HISTORY: usize = 120;
/// Minimum wall time between RAM/CPU/GPU/VRAM refreshes.
const STAT_INTERVAL: Duration = Duration::from_millis(400);
/// Linux kernel clock ticks per second (`sysconf(_SC_CLK_TCK)` is 100 on all
/// mainstream builds); used to turn /proc/self/stat jiffies into a CPU percent.
const CLK_TCK: f64 = 100.0;
/// A frame interval longer than this is treated as an idle gap (the canvas
/// simply was not being redrawn) and is not pushed as a sample.
const IDLE_GAP_MS: f32 = 500.0;
/// Frames averaged for the steady numeric readouts.
const SMOOTH_WINDOW: usize = 20;
/// Frames without input after which the latency readout is called idle.
const LATENCY_STALE_FRAMES: u32 = 30;
/// Instrumented stages plus the unmeasured residual.
const SEGMENTS: usize = STAGE_COUNT + 1;
/// Index of the residual segment inside a [`Breakdown`].
const IDLE: usize = STAGE_COUNT;

/// Per-frame stage shares, in milliseconds. Indices `0..STAGE_COUNT` follow
/// [`Stage::index`]; the last slot is the unaccounted remainder.
type Breakdown = [f32; SEGMENTS];

// -- Palette -------------------------------------------------------------------
// Categorical slots 1-6 of the validated dark-surface palette, in stack order,
// plus a neutral gray for the residual. Adjacent pairs clear the color-vision
// separation gate against this panel's surface, and the legend carries identity
// so nothing depends on hue alone.

const SEGMENT_COLORS: [(f64, f64, f64); SEGMENTS] = [
    (0.2235, 0.5294, 0.8980), // #3987e5 blue    - Input
    (0.8510, 0.3490, 0.1490), // #d95926 orange  - Brush + stamp
    (0.0980, 0.6196, 0.4392), // #199e70 aqua    - Composite
    (0.7882, 0.5216, 0.0000), // #c98500 yellow  - GPU wait
    (0.8353, 0.3176, 0.5059), // #d55181 magenta - Texture
    (0.0000, 0.5137, 0.0000), // #008300 green   - Snapshot
    (0.5647, 0.5216, 0.9137), // #9085e9 violet  - Timers / IO
    (0.5373, 0.5294, 0.5059), // #898781 gray    - Idle / GTK
];

/// Label for the residual: on a healthy frame this is mostly waiting for vsync,
/// so it is named for what it usually is rather than "unknown".
const IDLE_LABEL: &str = "Idle / GTK";

fn segment_label(i: usize) -> &'static str {
    Stage::ALL.get(i).map_or(IDLE_LABEL, |s| s.label())
}

// -- Panel layout (widget pixels) ---------------------------------------------

const OX: f64 = 12.0;
const OY: f64 = 12.0;
const PAD: f64 = 12.0;
const PANEL_W: f64 = 306.0;
const PANEL_H: f64 = 390.0;
const GRAPH_H: f64 = 54.0;
/// Frame budget marked on the latency chart (60 Hz).
const VSYNC_MS: f32 = 16.7;

#[derive(Clone, Copy, Default)]
struct SystemStats {
    ram_mb: Option<f64>,
    cpu_pct: Option<f64>,
    gpu_pct: Option<f64>,
    vram_mb: Option<f64>,
}

/// sysfs paths for the first DRM device that exposes usage counters.
#[derive(Clone, Default)]
struct GpuPaths {
    busy: Option<String>,
    vram_used: Option<String>,
}

pub(crate) struct PerfGraph {
    enabled: bool,
    /// Wall time per displayed frame (ms).
    total_ms: VecDeque<f32>,
    /// Where each frame's wall time went (ms per segment).
    breakdown: VecDeque<Breakdown>,
    /// Input arrival to present, per frame that carried input (ms).
    latency_ms: VecDeque<f32>,
    /// Frames drawn since the last input-carrying one, so a stalled reading is
    /// shown as idle rather than as a live number that stopped moving.
    frames_since_input: u32,
    /// GPU render time per frame (ms), from timestamp queries.
    render_ms: VecDeque<f32>,
    /// GPU present (dmabuf copy) time per frame (ms).
    present_ms: VecDeque<f32>,
    last_frame: Option<Instant>,
    stats: SystemStats,
    last_stat_sample: Option<Instant>,
    /// Previous (process jiffies, wall instant) for the CPU-percent delta.
    cpu_probe: Option<(f64, Instant)>,
    /// `None` until the DRM nodes have been probed once.
    gpu_paths: Option<GpuPaths>,
}

impl Default for PerfGraph {
    fn default() -> Self {
        Self {
            enabled: false,
            total_ms: VecDeque::with_capacity(HISTORY),
            breakdown: VecDeque::with_capacity(HISTORY),
            latency_ms: VecDeque::with_capacity(HISTORY),
            frames_since_input: 0,
            render_ms: VecDeque::with_capacity(HISTORY),
            present_ms: VecDeque::with_capacity(HISTORY),
            last_frame: None,
            stats: SystemStats::default(),
            last_stat_sample: None,
            cpu_probe: None,
            gpu_paths: None,
        }
    }
}

/// Bottom-right corner of the panel in widget pixels. The caller sizes the
/// overlay's cairo node to this instead of the whole widget - a canvas-sized
/// surface per frame would make the overlay a measurable part of what it is
/// measuring.
pub(crate) const fn panel_extent() -> (f32, f32) {
    #[allow(clippy::cast_possible_truncation)]
    {
        ((OX + PANEL_W + OX) as f32, (OY + PANEL_H + OY) as f32)
    }
}

impl Drop for PerfGraph {
    /// Closing a document with the overlay up must not leak its profiler claim.
    fn drop(&mut self) {
        if self.enabled {
            frame_profile::release();
        }
    }
}

impl PerfGraph {
    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Flip visibility. Resets the frame clock so the first frame after showing
    /// the panel doesn't record a giant idle interval. Takes or drops this
    /// panel's claim on the shared stage profiler.
    pub(crate) fn toggle(&mut self) {
        self.enabled = !self.enabled;
        self.last_frame = None;
        if self.enabled {
            frame_profile::retain();
        } else {
            frame_profile::release();
            self.total_ms.clear();
            self.breakdown.clear();
            self.latency_ms.clear();
            self.render_ms.clear();
            self.present_ms.clear();
        }
    }

    /// Record one displayed frame and refresh the system stats if due. Called at
    /// the top of every snapshot while the overlay is visible. `gpu` is the most
    /// recent frame's `(render_ms, present_ms)` GPU timings, if available, and
    /// `profile` is the CPU stage breakdown drained for the frame just ended.
    fn tick(&mut self, gpu: Option<(f32, f32)>, profile: FrameSample) {
        let now = Instant::now();
        if let Some(prev) = self.last_frame {
            let dt_ms = now.duration_since(prev).as_secs_f32() * 1000.0;
            if dt_ms <= IDLE_GAP_MS {
                push_sample(&mut self.total_ms, dt_ms);
                push_sample(&mut self.breakdown, split_frame(dt_ms, profile));
                if let Some(latency) = profile.input_latency_ms {
                    push_sample(&mut self.latency_ms, latency);
                    self.frames_since_input = 0;
                } else {
                    self.frames_since_input = self.frames_since_input.saturating_add(1);
                }
                let (render, present) = gpu.unwrap_or((0.0, 0.0));
                push_sample(&mut self.render_ms, render);
                push_sample(&mut self.present_ms, present);
            }
        }
        self.last_frame = Some(now);

        let due = self
            .last_stat_sample
            .is_none_or(|t| now.duration_since(t) >= STAT_INTERVAL);
        if due {
            self.sample_stats(now);
            self.last_stat_sample = Some(now);
        }
    }

    fn sample_stats(&mut self, now: Instant) {
        self.stats.ram_mb = read_rss_mb();

        if let Some(ticks) = read_process_ticks() {
            if let Some((prev_ticks, prev_now)) = self.cpu_probe {
                let wall = now.duration_since(prev_now).as_secs_f64();
                if wall > 0.0 {
                    let cpu_secs = (ticks - prev_ticks) / CLK_TCK;
                    self.stats.cpu_pct = Some((cpu_secs / wall * 100.0).max(0.0));
                }
            }
            self.cpu_probe = Some((ticks, now));
        }

        if self.gpu_paths.is_none() {
            self.gpu_paths = Some(find_gpu_paths());
        }
        if let Some(paths) = &self.gpu_paths {
            self.stats.gpu_pct = paths.busy.as_deref().and_then(read_f64);
            self.stats.vram_mb = paths
                .vram_used
                .as_deref()
                .and_then(read_f64)
                .map(|bytes| bytes / (1024.0 * 1024.0));
        }
    }

    /// Mean per segment over the recent window, for the legend readouts.
    fn smoothed_breakdown(&self) -> Breakdown {
        let n = SMOOTH_WINDOW.min(self.breakdown.len());
        let mut out = [0.0f32; SEGMENTS];
        if n == 0 {
            return out;
        }
        for frame in self.breakdown.iter().rev().take(n) {
            for (slot, v) in out.iter_mut().zip(frame.iter()) {
                *slot += v;
            }
        }
        for slot in &mut out {
            #[allow(clippy::cast_precision_loss)]
            {
                *slot /= n as f32;
            }
        }
        out
    }

    /// Tick the counters and paint the panel. `cr` is a full-widget cairo
    /// context appended by the paintable snapshot.
    pub(crate) fn render(
        &mut self,
        cr: &gtk::cairo::Context,
        gpu: Option<(f32, f32)>,
        profile: FrameSample,
    ) {
        self.tick(gpu, profile);

        // Panel background + subtle border. Kept mostly opaque so the chart
        // colors read the same over light and dark canvas areas.
        rounded_rect(cr, OX, OY, PANEL_W, PANEL_H, 9.0);
        cr.set_source_rgba(0.055, 0.055, 0.07, 0.93);
        cr.fill_preserve().ok();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.12);
        cr.set_line_width(1.0);
        cr.stroke().ok();

        let x = OX + PAD;
        let w = PANEL_W - 2.0 * PAD;
        let mut y = OY + PAD + 11.0;

        let avg = mean(&self.total_ms, SMOOTH_WINDOW);
        text(cr, x, y, "PERFORMANCE", 10.0, 0.55);
        let fps = if avg > 0.0 { 1000.0 / avg } else { 0.0 };
        text_right(cr, x + w, y, &format!("{fps:.0} FPS"), 10.0, 0.55);
        y += 19.0;

        y = self.draw_latency_section(cr, x, y, w, avg);
        y += 10.0;
        y = self.draw_breakdown_section(cr, x, y, w);
        y += 10.0;
        self.draw_stat_rows(cr, x, y, w);
    }

    /// Total wall time per frame, with the vsync budget marked.
    fn draw_latency_section(
        &self,
        cr: &gtk::cairo::Context,
        x: f64,
        mut y: f64,
        w: f64,
        avg: f32,
    ) -> f64 {
        text(cr, x, y, "Total frame latency", 11.0, 0.85);
        text_right(cr, x + w, y, &format!("{avg:.2} ms"), 11.0, 0.95);
        y += 6.0;

        let data: Vec<f32> = self.total_ms.iter().copied().collect();
        let max = data.iter().copied().fold(0.0_f32, f32::max);
        let scale = (max * 1.15).max(VSYNC_MS * 1.2);
        draw_plot(cr, x, y, w, GRAPH_H, &data, scale, VSYNC_MS);
        y += GRAPH_H + 13.0;

        // Worst-case matters more than the mean for perceived smoothness, so
        // the spread gets its own row.
        let min = data.iter().copied().fold(f32::INFINITY, f32::min);
        let min = if min.is_finite() { min } else { 0.0 };
        let stats = format!(
            "min {min:.1}  avg {avg:.1}  p99 {:.1}  max {max:.1} ms",
            percentile(&data, 0.99),
        );
        text(cr, x, y, &stats, 10.0, 0.6);
        y += 14.0;

        // Only meaningful while input is actually arriving; past that the deque
        // still holds the last stroke's numbers, which would read as live.
        let label = if self.latency_ms.is_empty() || self.frames_since_input > LATENCY_STALE_FRAMES
        {
            "Input to present   idle".to_string()
        } else {
            format!(
                "Input to present   {:.1} ms",
                mean(&self.latency_ms, SMOOTH_WINDOW)
            )
        };
        text(cr, x, y, &label, 10.0, 0.6);
        y + 8.0
    }

    /// The same frames as the latency chart, split into stage shares.
    fn draw_breakdown_section(
        &self,
        cr: &gtk::cairo::Context,
        x: f64,
        mut y: f64,
        w: f64,
    ) -> f64 {
        // Idle is the bulk of a healthy frame, so the busy figure is what the
        // eye should land on - the stack alone makes 95% idle look alarming.
        let means = self.smoothed_breakdown();
        let busy: f32 = means[..STAGE_COUNT].iter().sum();
        text(cr, x, y, "Frame breakdown", 11.0, 0.85);
        text_right(cr, x + w, y, &format!("busy {busy:.2} ms"), 11.0, 0.95);
        y += 6.0;

        let frames: Vec<Breakdown> = self.breakdown.iter().copied().collect();
        draw_stacked(cr, x, y, w, GRAPH_H, &frames);
        y += GRAPH_H + 14.0;

        // Legend: swatch, stage, mean ms and share - one row each, so the
        // longest label still has room for its numbers.
        let total: f32 = means.iter().sum();
        let row_h = 14.0;
        for (i, value) in means.iter().enumerate() {
            let cy = y + row_h * f64::from(u32::try_from(i).unwrap_or(0));
            let (r, g, b) = SEGMENT_COLORS[i];
            cr.set_source_rgb(r, g, b);
            rounded_rect(cr, x, cy - 7.0, 8.0, 8.0, 2.0);
            cr.fill().ok();
            text(cr, x + 14.0, cy, segment_label(i), 10.5, 0.8);
            let share = if total > 0.0 { value / total * 100.0 } else { 0.0 };
            text_right(cr, x + w - 44.0, cy, &format!("{value:.2} ms"), 10.5, 0.75);
            text_right(cr, x + w, cy, &format!("{share:.0}%"), 10.5, 0.9);
        }
        #[allow(clippy::cast_precision_loss)]
        {
            y + row_h * SEGMENTS as f64
        }
    }

    /// GPU timestamp readouts and the /proc + sysfs system stats.
    fn draw_stat_rows(&self, cr: &gtk::cairo::Context, x: f64, mut y: f64, w: f64) {
        let last = |q: &VecDeque<f32>| q.back().copied().unwrap_or(0.0);
        text(cr, x, y, "GPU passes", 10.0, 0.55);
        text_right(
            cr,
            x + w,
            y,
            &format!(
                "render {:.2} ms   present {:.2} ms",
                last(&self.render_ms),
                last(&self.present_ms)
            ),
            10.0,
            0.75,
        );
        y += 18.0;

        let cols = [
            ("VRAM", fmt_mb(self.stats.vram_mb)),
            ("RAM", fmt_mb(self.stats.ram_mb)),
            ("CPU", fmt_pct(self.stats.cpu_pct)),
            ("GPU", fmt_pct(self.stats.gpu_pct)),
        ];
        let col_w = w / 4.0;
        for (i, (label, value)) in cols.iter().enumerate() {
            let cx = x + col_w * f64::from(u32::try_from(i).unwrap_or(0));
            text(cr, cx, y, label, 9.5, 0.5);
            text(cr, cx, y + 15.0, value, 12.0, 0.95);
        }
    }
}

/// Split one frame's wall time into stage shares. Instrumented spans are taken
/// as measured; the remainder (GTK's own render, the compositor handoff, and
/// above all the wait for the next vsync) becomes the idle residual. On a
/// healthy 60Hz frame doing a millisecond of work, that residual is *supposed*
/// to be ~95% - it shrinking is what signals trouble. Spans can overshoot the
/// slightly when a stage straddles the sample boundary, so shares are scaled
/// back to fit rather than allowed to exceed the total.
fn split_frame(dt_ms: f32, profile: FrameSample) -> Breakdown {
    let mut out = [0.0f32; SEGMENTS];
    let measured = profile.measured_ms();
    if measured > dt_ms && measured > 0.0 {
        let scale = dt_ms / measured;
        for (slot, v) in out.iter_mut().zip(profile.stages.iter()) {
            *slot = v * scale;
        }
        return out;
    }
    out[..STAGE_COUNT].copy_from_slice(&profile.stages);
    out[IDLE] = dt_ms - measured;
    out
}

/// Push a value into a fixed-capacity rolling buffer (drops the oldest).
fn push_sample<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() == HISTORY {
        q.pop_front();
    }
    q.push_back(v);
}

/// Mean of the most recent `n` samples.
fn mean(q: &VecDeque<f32>, n: usize) -> f32 {
    let n = n.min(q.len());
    if n == 0 {
        return 0.0;
    }
    let sum: f32 = q.iter().rev().take(n).sum();
    #[allow(clippy::cast_precision_loss)]
    {
        sum / n as f32
    }
}

/// Nearest-rank percentile of `values` (order independent - copies and sorts).
fn percentile(values: &[f32], q: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    let idx = ((sorted.len() as f32 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// -- cairo helpers -------------------------------------------------------------

fn text(cr: &gtk::cairo::Context, x: f64, y: f64, s: &str, size: f64, alpha: f64) {
    cr.set_font_size(size);
    cr.set_source_rgba(1.0, 1.0, 1.0, alpha);
    cr.move_to(x, y);
    cr.show_text(s).ok();
}

fn text_right(cr: &gtk::cairo::Context, right_x: f64, y: f64, s: &str, size: f64, alpha: f64) {
    cr.set_font_size(size);
    let w = cr.text_extents(s).map_or(0.0, |e| e.width());
    cr.set_source_rgba(1.0, 1.0, 1.0, alpha);
    cr.move_to(right_x - w, y);
    cr.show_text(s).ok();
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, PI * 1.5);
    cr.close_path();
}

fn plot_frame(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64) {
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.05);
    cr.rectangle(x, y, w, h);
    cr.fill().ok();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.15);
    cr.set_line_width(1.0);
    cr.rectangle(x + 0.5, y + 0.5, w - 1.0, h - 1.0);
    cr.stroke().ok();
}

/// Horizontal position of sample `i` of `len`, newest anchored to the right
/// edge so the trace scrolls left over time.
fn sample_x(x: f64, w: f64, i: usize, len: usize) -> f64 {
    let from_right = f64::from(u32::try_from(len - 1 - i).unwrap_or(0));
    let denom = f64::from(u32::try_from(HISTORY - 1).unwrap_or(1));
    x + w - (from_right / denom) * w
}

/// Filled line plot of `values` (oldest..newest) into a framed box, with a
/// dashed reference line at `budget` milliseconds.
fn draw_plot(
    cr: &gtk::cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    values: &[f32],
    scale_max: f32,
    budget: f32,
) {
    plot_frame(cr, x, y, w, h);
    if scale_max <= 0.0 {
        return;
    }

    let plot_y = |v: f32| {
        let norm = f64::from((v / scale_max).clamp(0.0, 1.0));
        y + h - 2.0 - norm * (h - 4.0)
    };

    // Frame budget: anything above this line missed a 60 Hz vsync.
    if budget > 0.0 && budget < scale_max {
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.28);
        cr.set_line_width(1.0);
        cr.set_dash(&[3.0, 3.0], 0.0);
        cr.move_to(x + 1.0, plot_y(budget).round() + 0.5);
        cr.line_to(x + w - 1.0, plot_y(budget).round() + 0.5);
        cr.stroke().ok();
        cr.set_dash(&[], 0.0);
    }

    if values.len() < 2 {
        return;
    }
    let len = values.len();
    let trace = |cr: &gtk::cairo::Context| {
        for (i, v) in values.iter().enumerate() {
            let px = sample_x(x, w, i, len);
            let py = plot_y(*v);
            if i == 0 {
                cr.move_to(px, py);
            } else {
                cr.line_to(px, py);
            }
        }
    };

    // Soft fill under the trace, then the trace itself on top.
    cr.set_source_rgba(0.2235, 0.5294, 0.8980, 0.22);
    trace(cr);
    cr.line_to(sample_x(x, w, len - 1, len), y + h - 2.0);
    cr.line_to(sample_x(x, w, 0, len), y + h - 2.0);
    cr.close_path();
    cr.fill().ok();

    cr.set_source_rgba(0.427, 0.667, 0.949, 0.95);
    cr.set_line_width(1.5);
    trace(cr);
    cr.stroke().ok();
}

/// 100% stacked column chart: one column per frame, each split into stage
/// shares of that frame's own total. Column heights are normalised, so the
/// chart shows composition over time rather than magnitude - the latency chart
/// above carries the magnitude.
fn draw_stacked(
    cr: &gtk::cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    frames: &[Breakdown],
) {
    plot_frame(cr, x, y, w, h);
    if frames.is_empty() {
        return;
    }

    let inner_y = y + 1.0;
    let inner_h = h - 2.0;
    let denom = f64::from(u32::try_from(HISTORY).unwrap_or(1));
    let step = w / denom;
    let len = frames.len();

    for (i, frame) in frames.iter().enumerate() {
        let total: f32 = frame.iter().sum();
        if total <= 0.0 {
            continue;
        }
        // Snap both edges to whole pixels: at ~2px per column, unrounded edges
        // antialias into visible vertical striping across the whole chart.
        let from_right = f64::from(u32::try_from(len - 1 - i).unwrap_or(0));
        let x0 = (x + w - (from_right + 1.0) * step).round();
        let x1 = (x + w - from_right * step).round();
        let col_w = (x1 - x0).max(1.0);
        if x0 + col_w < x {
            continue;
        }
        // Stack upward from the baseline so segment 0 sits at the bottom.
        let mut cursor = inner_y + inner_h;
        for (seg, value) in frame.iter().enumerate() {
            let seg_h = f64::from(value / total) * inner_h;
            if seg_h <= 0.0 {
                continue;
            }
            let (r, g, b) = SEGMENT_COLORS[seg];
            cr.set_source_rgb(r, g, b);
            cr.rectangle(x0, cursor - seg_h, col_w, seg_h);
            cr.fill().ok();
            cursor -= seg_h;
        }
    }

    // Re-stroke the frame so the columns don't overhang the border.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.15);
    cr.set_line_width(1.0);
    cr.rectangle(x + 0.5, y + 0.5, w - 1.0, h - 1.0);
    cr.stroke().ok();
}

fn fmt_mb(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".to_string(), |mb| format!("{mb:.0} MB"))
}

fn fmt_pct(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".to_string(), |p| format!("{p:.0}%"))
}

// -- /proc + sysfs probes ------------------------------------------------------

fn read_f64(path: &str) -> Option<f64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Resident set size of this process, in MB, from /proc/self/status.
fn read_rss_mb() -> Option<f64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024.0);
        }
    }
    None
}

/// utime + stime of this process in clock ticks, from /proc/self/stat. The comm
/// field (in parens) can contain spaces, so fields are read after the last ')'.
fn read_process_ticks() -> Option<f64> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let after = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    // After ')', index 0 is `state` (field 3); utime is field 14 -> index 11,
    // stime is field 15 -> index 12.
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Find the first DRM card exposing AMD-style usage counters. `gpu_busy_percent`
/// and `mem_info_vram_used` are absent on non-AMD drivers, in which case the
/// corresponding readouts show "n/a".
fn find_gpu_paths() -> GpuPaths {
    for n in 0..8 {
        let base = format!("/sys/class/drm/card{n}/device");
        let busy = format!("{base}/gpu_busy_percent");
        let vram = format!("{base}/mem_info_vram_used");
        let has_busy = Path::new(&busy).exists();
        let has_vram = Path::new(&vram).exists();
        if has_busy || has_vram {
            return GpuPaths {
                busy: has_busy.then_some(busy),
                vram_used: has_vram.then_some(vram),
            };
        }
    }
    GpuPaths::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(stages: [f32; STAGE_COUNT]) -> FrameSample {
        FrameSample {
            stages,
            input_latency_ms: None,
        }
    }

    #[test]
    fn residual_absorbs_unmeasured_time() {
        let mut stages = [0.0f32; STAGE_COUNT];
        stages[..3].copy_from_slice(&[1.0, 2.0, 3.0]);
        let split = split_frame(16.0, sample(stages));
        assert!((split[IDLE] - 10.0).abs() < 1e-4);
        assert!((split.iter().sum::<f32>() - 16.0).abs() < 1e-4);
    }

    #[test]
    fn overshooting_spans_are_scaled_to_the_interval() {
        // Spans straddling the sample boundary can exceed the frame interval;
        // the stack must still sum to it rather than overflow the column.
        let mut stages = [0.0f32; STAGE_COUNT];
        stages[..2].copy_from_slice(&[10.0, 10.0]);
        let split = split_frame(10.0, sample(stages));
        assert!((split.iter().sum::<f32>() - 10.0).abs() < 1e-4);
        assert!(split[IDLE].abs() < 1e-6);
        assert!((split[0] - 5.0).abs() < 1e-4);
    }

    #[test]
    fn every_segment_has_a_color_and_label() {
        assert_eq!(SEGMENT_COLORS.len(), SEGMENTS);
        assert_eq!(segment_label(IDLE), IDLE_LABEL);
        for (i, stage) in Stage::ALL.iter().enumerate() {
            assert_eq!(segment_label(i), stage.label());
        }
    }

    #[test]
    fn percentile_picks_the_nearest_rank() {
        let values = [1.0, 2.0, 3.0, 4.0, 100.0];
        assert!((percentile(&values, 0.99) - 100.0).abs() < 1e-6);
        assert!((percentile(&values, 0.0) - 1.0).abs() < 1e-6);
        assert!(percentile(&[], 0.5).abs() < 1e-6);
    }

    #[test]
    fn rolling_buffer_is_capped() {
        let mut q: VecDeque<f32> = VecDeque::new();
        for i in 0..(HISTORY + 30) {
            #[allow(clippy::cast_precision_loss)]
            push_sample(&mut q, i as f32);
        }
        assert_eq!(q.len(), HISTORY);
        #[allow(clippy::cast_precision_loss)]
        let newest = (HISTORY + 29) as f32;
        assert!((q.back().copied().unwrap_or_default() - newest).abs() < 1e-6);
    }
}
