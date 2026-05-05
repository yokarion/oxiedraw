//! On-canvas performance overlay (toggle with F3 via the `perf-graph` action).
//!
//! Draws a semi-transparent dark panel in the top-left corner with two white
//! line graphs (frame time + FPS) and a row of VRAM / RAM / CPU / GPU readouts.
//! A sample is pushed once per canvas redraw (each paintable snapshot), so idle
//! time between renders is ignored - the graphs only reflect frames that were
//! actually drawn. System stats are read from /proc and the DRM sysfs nodes and
//! refreshed at most a couple of times a second.

use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

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

// -- Panel layout (widget pixels) ---------------------------------------------

const OX: f64 = 12.0;
const OY: f64 = 12.0;
const PAD: f64 = 10.0;
const PANEL_W: f64 = 234.0;
const PANEL_H: f64 = 198.0;
const GRAPH_H: f64 = 44.0;

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
    frame_ms: VecDeque<f32>,
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
            frame_ms: VecDeque::with_capacity(HISTORY),
            last_frame: None,
            stats: SystemStats::default(),
            last_stat_sample: None,
            cpu_probe: None,
            gpu_paths: None,
        }
    }
}

impl PerfGraph {
    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Flip visibility. Resets the frame clock so the first frame after showing
    /// the panel doesn't record a giant idle interval.
    pub(crate) fn toggle(&mut self) {
        self.enabled = !self.enabled;
        self.last_frame = None;
        if !self.enabled {
            self.frame_ms.clear();
        }
    }

    /// Record one rendered frame and refresh the system stats if due. Called at
    /// the top of every snapshot while the overlay is visible.
    fn tick(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_frame {
            let dt_ms = now.duration_since(prev).as_secs_f32() * 1000.0;
            if dt_ms <= IDLE_GAP_MS {
                if self.frame_ms.len() == HISTORY {
                    self.frame_ms.pop_front();
                }
                self.frame_ms.push_back(dt_ms);
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

    /// Mean of the most recent frame samples, for a steady on-screen readout.
    fn smoothed_ms(&self) -> f32 {
        let n = 20.min(self.frame_ms.len());
        if n == 0 {
            return 0.0;
        }
        let sum: f32 = self.frame_ms.iter().rev().take(n).sum();
        sum / n as f32
    }

    /// Tick the counters and paint the panel. `cr` is a full-widget cairo
    /// context appended by the paintable snapshot.
    pub(crate) fn render(&mut self, cr: &gtk::cairo::Context) {
        self.tick();

        // Panel background + subtle border.
        rounded_rect(cr, OX, OY, PANEL_W, PANEL_H, 8.0);
        cr.set_source_rgba(0.05, 0.05, 0.07, 0.78);
        cr.fill_preserve().ok();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.12);
        cr.set_line_width(1.0);
        cr.stroke().ok();

        let inner_x = OX + PAD;
        let inner_w = PANEL_W - 2.0 * PAD;
        let mut y = OY + PAD + 11.0;

        text(cr, inner_x, y, "PERFORMANCE GRAPH", 10.0, 0.6);
        y += 17.0;

        // Frame-time graph.
        let frame_ms = self.smoothed_ms();
        text(cr, inner_x, y, "Frame time", 11.0, 0.85);
        text_right(
            cr,
            inner_x + inner_w,
            y,
            &format!("{frame_ms:.2} ms"),
            11.0,
            0.95,
        );
        y += 6.0;
        let frames: Vec<f32> = self.frame_ms.iter().copied().collect();
        let frame_max = frames.iter().copied().fold(0.0_f32, f32::max);
        draw_plot(cr, inner_x, y, inner_w, GRAPH_H, &frames, (frame_max * 1.2).max(33.4));
        y += GRAPH_H + 8.0;

        // FPS graph (derived from the same samples).
        let fps_now = if frame_ms > 0.0 { 1000.0 / frame_ms } else { 0.0 };
        text(cr, inner_x, y, "FPS", 11.0, 0.85);
        text_right(cr, inner_x + inner_w, y, &format!("{fps_now:.0}"), 11.0, 0.95);
        y += 6.0;
        let fps: Vec<f32> = frames
            .iter()
            .map(|&ms| if ms > 0.0 { 1000.0 / ms } else { 0.0 })
            .collect();
        let fps_max = fps.iter().copied().fold(0.0_f32, f32::max);
        draw_plot(cr, inner_x, y, inner_w, GRAPH_H, &fps, (fps_max * 1.2).max(60.0));
        y += GRAPH_H + 10.0;

        // Stat columns: VRAM / RAM / CPU / GPU.
        let cols = [
            ("VRAM", fmt_mb(self.stats.vram_mb)),
            ("RAM", fmt_mb(self.stats.ram_mb)),
            ("CPU", fmt_pct(self.stats.cpu_pct)),
            ("GPU", fmt_pct(self.stats.gpu_pct)),
        ];
        let col_w = inner_w / 4.0;
        for (i, (label, value)) in cols.iter().enumerate() {
            let cx = inner_x + col_w * i as f64;
            text(cr, cx, y, label, 9.5, 0.5);
            text(cr, cx, y + 15.0, value, 12.0, 0.95);
        }
    }
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

/// White line plot of `values` (oldest..newest) into a framed box. The newest
/// sample is anchored to the right edge so the trace scrolls left over time.
fn draw_plot(
    cr: &gtk::cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    values: &[f32],
    scale_max: f32,
) {
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.05);
    cr.rectangle(x, y, w, h);
    cr.fill().ok();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.15);
    cr.set_line_width(1.0);
    cr.rectangle(x + 0.5, y + 0.5, w - 1.0, h - 1.0);
    cr.stroke().ok();

    if values.len() < 2 || scale_max <= 0.0 {
        return;
    }

    let denom = (HISTORY - 1) as f64;
    let len = values.len();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
    cr.set_line_width(1.0);
    for (i, v) in values.iter().enumerate() {
        let from_right = (len - 1 - i) as f64;
        let px = x + w - (from_right / denom) * w;
        let norm = f64::from((v / scale_max).clamp(0.0, 1.0));
        let py = y + h - 2.0 - norm * (h - 4.0);
        if i == 0 {
            cr.move_to(px, py);
        } else {
            cr.line_to(px, py);
        }
    }
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
