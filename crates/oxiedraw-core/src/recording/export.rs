//! Replays a recording into an H.264 video, an animated AVIF or a ProRes video
//! (all piped through ffmpeg), or a folder of PNG frames. Frames are composited
//! over the chosen background here; frames recorded at another size (a crop, a
//! scale change) are fitted into the last frame's size so the whole timelapse
//! shares one resolution.
//!
//! In the AVIF, SVT-AV1 encodes the colour and libaom the alpha plane: AVIF
//! wants alpha as a single-plane image, which SVT-AV1 (4:2:0 only) can't encode.

use std::borrow::Cow;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use oxiedraw_utils::pixels::scale_bgra8_bilinear;
use serde::{Deserialize, Serialize};

use super::RecordingError;
use super::codec::{Frame, FrameDecoder, FrameHeader, FrameKind};
use super::segments::{FrameReader, SegmentSource};
use crate::enum_meta::EnumMeta;

/// "Include canvas" pads the artwork by this share of its width and height.
const CANVAS_MARGIN: f32 = 0.2;
const CHECKER_CELL: u32 = 64;
const CHECKER_DARK: u8 = 0xC7;
const CHECKER_LIGHT: u8 = 0xEB;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    H264,
    Avif,
    ProRes,
    ImageSequence,
}

impl OutputFormat {
    /// One file encoded by ffmpeg, as opposed to PNG frames written here.
    #[must_use]
    pub const fn uses_ffmpeg(self) -> bool {
        !matches!(self, Self::ImageSequence)
    }

    #[must_use]
    pub const fn supports_alpha(self) -> bool {
        !matches!(self, Self::H264)
    }

    /// What the file is for, shown under the format row.
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::H264 => "MP4 that plays everywhere (no alpha)",
            Self::Avif => "AV1 in an AVIF, plays in web browsers",
            Self::ProRes => "ProRes 4444 MOV, for video editors",
            Self::ImageSequence => "One PNG file per frame",
        }
    }

    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::H264 => "mp4",
            Self::Avif => "avif",
            Self::ProRes => "mov",
            Self::ImageSequence => "png",
        }
    }

    /// ffmpeg encoders the export needs.
    const fn encoders(self, alpha: bool) -> &'static [&'static str] {
        match (self, alpha) {
            (Self::H264, _) => &["libx264"],
            (Self::Avif, false) => &["libsvtav1"],
            (Self::Avif, true) => &["libsvtav1", "libaom-av1"],
            (Self::ProRes, _) => &["prores_ks"],
            (Self::ImageSequence, _) => &[],
        }
    }
}

impl EnumMeta for OutputFormat {
    const ALL: &'static [Self] = &[Self::H264, Self::Avif, Self::ProRes, Self::ImageSequence];

    fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264 Video",
            Self::Avif => "Animated AVIF",
            Self::ProRes => "ProRes Video",
            Self::ImageSequence => "PNG Image Sequence",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    Low,
    #[default]
    Medium,
    High,
}

impl Compression {
    /// The codec's quality setting: CRF, or the quantiser for ProRes. Each codec
    /// has its own scale; these land on similar-looking output.
    const fn level(self, format: OutputFormat) -> &'static str {
        let low_medium_high = match format {
            OutputFormat::Avif => ["28", "35", "42"],
            OutputFormat::ProRes => ["4", "8", "13"],
            OutputFormat::H264 | OutputFormat::ImageSequence => ["18", "23", "28"],
        };
        low_medium_high[self as usize]
    }

    const fn png(self) -> png::Compression {
        match self {
            Self::Low => png::Compression::Fast,
            Self::Medium => png::Compression::Default,
            Self::High => png::Compression::Best,
        }
    }
}

impl EnumMeta for Compression {
    const ALL: &'static [Self] = &[Self::Low, Self::Medium, Self::High];

    fn label(self) -> &'static str {
        match self {
            Self::Low => "Low (bigger file size)",
            Self::Medium => "Medium (medium file size)",
            Self::High => "High (smaller file size)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FrameRate {
    Twelve,
    #[default]
    TwentyFour,
    Sixty,
}

impl FrameRate {
    #[must_use]
    pub const fn fps(self) -> u32 {
        match self {
            Self::Twelve => 12,
            Self::TwentyFour => 24,
            Self::Sixty => 60,
        }
    }
}

impl EnumMeta for FrameRate {
    const ALL: &'static [Self] = &[Self::Twelve, Self::TwentyFour, Self::Sixty];

    fn label(self) -> &'static str {
        match self {
            Self::Twelve => "12 fps",
            Self::TwentyFour => "24 fps",
            Self::Sixty => "60 fps",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    Alpha,
    Checkerboard,
    Black,
    #[default]
    White,
    Gray,
}

impl Background {
    /// Backgrounds a format can carry; H.264 has no alpha channel.
    #[must_use]
    pub fn choices(format: OutputFormat) -> &'static [Self] {
        if format.supports_alpha() { Self::ALL } else { &Self::ALL[1..] }
    }

    const fn solid(self) -> Option<[u8; 3]> {
        match self {
            Self::Alpha | Self::Checkerboard => None,
            Self::Black => Some([0, 0, 0]),
            Self::White => Some([255, 255, 255]),
            Self::Gray => Some([128, 128, 128]),
        }
    }
}

impl EnumMeta for Background {
    const ALL: &'static [Self] = &[Self::Alpha, Self::Checkerboard, Self::Black, Self::White, Self::Gray];

    fn label(self) -> &'static str {
        match self {
            Self::Alpha => "Alpha",
            Self::Checkerboard => "Checkerboard Tiles",
            Self::Black => "Black",
            Self::White => "White",
            Self::Gray => "Gray",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExportOptions {
    #[serde(default)]
    pub format: OutputFormat,
    #[serde(default)]
    pub compression: Compression,
    #[serde(default)]
    pub frame_rate: FrameRate,
    #[serde(default)]
    pub include_canvas: bool,
    #[serde(default)]
    pub background: Background,
    #[serde(default)]
    pub metadata: bool,
}

impl ExportOptions {
    /// The background actually used: H.264 falls back to white for alpha.
    #[must_use]
    pub fn effective_background(&self) -> Background {
        if Background::choices(self.format).contains(&self.background) {
            self.background
        } else {
            Background::White
        }
    }

    #[must_use]
    pub fn has_alpha(&self) -> bool {
        self.effective_background() == Background::Alpha
    }
}

pub enum ExportTarget {
    Video { path: PathBuf, ffmpeg: PathBuf },
    /// A new folder, created by the export, that receives `<prefix>_00001.png`...
    Sequence { dir: PathBuf, prefix: String },
}

pub struct ExportJob {
    pub sources: Vec<SegmentSource>,
    pub options: ExportOptions,
    pub target: ExportTarget,
    /// The colour around the canvas in the app, used by "include canvas".
    pub margin_rgb: [u8; 3],
    pub title: String,
    pub software: String,
}

/// Shared with the UI thread while an export runs.
#[derive(Default)]
pub struct ExportProgress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

/// Run the export to completion; returns the number of frames written. On
/// failure or cancel, the partial output is removed.
pub fn run(job: &ExportJob, progress: &ExportProgress) -> Result<u64, RecordingError> {
    let scan = scan(&job.sources)?;
    progress.total.store(scan.frames, Ordering::Relaxed);
    let layout = Layout::new(&scan, job.options, job.margin_rgb);
    let meta = job.options.metadata.then(|| Metadata::new(job, &scan));

    let mut sink = match &job.target {
        ExportTarget::Video { path, ffmpeg } => Sink::video(ffmpeg, path, &layout, job.options, meta.as_ref())?,
        ExportTarget::Sequence { dir, prefix } => {
            std::fs::create_dir(dir)?;
            Sink::Sequence { dir: dir.clone(), prefix: prefix.clone(), written: Vec::new() }
        }
    };
    match replay(job, &layout, meta.as_ref(), &mut sink, progress) {
        Ok(written) => sink.finish().map(|()| written),
        // The pipe broke because ffmpeg quit; its own message says why.
        Err(e @ RecordingError::Ffmpeg(_)) => Err(sink.finish().err().unwrap_or(e)),
        Err(e) => {
            sink.abandon();
            Err(e)
        }
    }
}

struct Scan {
    frames: u64,
    width: u32,
    height: u32,
    halvings: u8,
    first_ms: u64,
    last_ms: u64,
}

fn scan(sources: &[SegmentSource]) -> Result<Scan, RecordingError> {
    let mut reader = FrameReader::new(sources.to_vec());
    let mut last: Option<FrameHeader> = None;
    let mut frames = 0;
    let mut first_ms = 0;
    while let Some(header) = reader.next_header()? {
        reader.skip_payload(&header)?;
        if frames == 0 {
            first_ms = header.time_ms;
        }
        frames += 1;
        last = Some(header);
    }
    let last = last.ok_or(RecordingError::Empty)?;
    Ok(Scan {
        frames,
        width: last.width,
        height: last.height,
        halvings: last.halvings,
        first_ms,
        last_ms: last.time_ms,
    })
}

fn replay(
    job: &ExportJob,
    layout: &Layout,
    meta: Option<&Metadata>,
    sink: &mut Sink,
    progress: &ExportProgress,
) -> Result<u64, RecordingError> {
    let mut reader = FrameReader::new(job.sources.clone());
    let mut decoder = FrameDecoder::default();
    let mut composed: Vec<u8> = Vec::new();
    let mut written = 0;
    while let Some(header) = reader.next_header()? {
        if progress.cancel.load(Ordering::Relaxed) {
            return Err(RecordingError::Cancelled);
        }
        let payload = reader.read_payload(&header)?;
        let reuse = header.kind == FrameKind::Repeat && !composed.is_empty();
        match decoder.apply(&header, &payload)? {
            Some(_) if reuse => {}
            Some(frame) => composed = layout.compose(frame),
            None if composed.is_empty() => {
                progress.done.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // A gap in the stream: hold the last good frame rather than drop time.
            None => {}
        }
        written += 1;
        sink.write(&composed, layout, job.options.compression, meta, header.time_ms, written)?;
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    Ok(written)
}

/// Where the artwork sits in the output frame and what surrounds it.
struct Layout {
    out_w: u32,
    out_h: u32,
    art_x: u32,
    art_y: u32,
    art_w: u32,
    art_h: u32,
    outside: [u8; 4],
    background: Background,
    checker_cell: u32,
}

impl Layout {
    fn new(scan: &Scan, options: ExportOptions, margin_rgb: [u8; 3]) -> Self {
        let (art_w, art_h) = (scan.width.max(1), scan.height.max(1));
        let background = options.effective_background();
        let (mut out_w, mut out_h) = if options.include_canvas {
            (grow(art_w), grow(art_h))
        } else {
            (art_w, art_h)
        };
        // yuv420p needs even dimensions.
        if options.format.uses_ffmpeg() {
            out_w += out_w % 2;
            out_h += out_h % 2;
        }
        let outside = if options.include_canvas {
            [margin_rgb[0], margin_rgb[1], margin_rgb[2], 255]
        } else {
            match background {
                Background::Alpha => [0, 0, 0, 0],
                Background::Checkerboard => [CHECKER_LIGHT, CHECKER_LIGHT, CHECKER_LIGHT, 255],
                other => {
                    let [r, g, b] = other.solid().unwrap_or([255, 255, 255]);
                    [r, g, b, 255]
                }
            }
        };
        let art_x = if options.include_canvas { (out_w - art_w) / 2 } else { 0 };
        let art_y = if options.include_canvas { (out_h - art_h) / 2 } else { 0 };
        Self {
            out_w,
            out_h,
            art_x,
            art_y,
            art_w,
            art_h,
            outside,
            background,
            checker_cell: (CHECKER_CELL >> scan.halvings).max(2),
        }
    }

    fn has_alpha(&self) -> bool {
        self.background == Background::Alpha
    }

    /// Composite `frame` into a straight RGBA8 buffer of the output size.
    fn compose(&self, frame: &Frame) -> Vec<u8> {
        let art = self.fit(frame);
        let (ow, oh) = (self.out_w as usize, self.out_h as usize);
        let mut out = Vec::with_capacity(ow * oh * 4);
        for y in 0..self.out_h {
            for x in 0..self.out_w {
                let inside = x >= self.art_x
                    && y >= self.art_y
                    && x < self.art_x + self.art_w
                    && y < self.art_y + self.art_h;
                if !inside {
                    out.extend_from_slice(&self.outside);
                    continue;
                }
                let (ax, ay) = (x - self.art_x, y - self.art_y);
                let i = (ay as usize * self.art_w as usize + ax as usize) * 4;
                let px = [art[i], art[i + 1], art[i + 2], art[i + 3]];
                out.extend_from_slice(&self.over_background(px, ax, ay));
            }
        }
        out
    }

    fn over_background(&self, bgra: [u8; 4], x: u32, y: u32) -> [u8; 4] {
        let [b, g, r, a] = bgra;
        let base = match self.background {
            Background::Alpha => {
                if a == 0 {
                    return [0, 0, 0, 0];
                }
                let straight = |c: u8| ((u16::from(c) * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8;
                return [straight(r), straight(g), straight(b), a];
            }
            Background::Checkerboard => {
                let dark = ((x / self.checker_cell) ^ (y / self.checker_cell)) & 1 == 0;
                let v = if dark { CHECKER_DARK } else { CHECKER_LIGHT };
                [v, v, v]
            }
            other => other.solid().unwrap_or([255, 255, 255]),
        };
        // Premultiplied OVER in byte space, matching the still-image export.
        let over = |c: u8, bg: u8| {
            let under = u16::from(bg) * u16::from(255 - a) / 255;
            (u16::from(c) + under).min(255) as u8
        };
        [over(r, base[0]), over(g, base[1]), over(b, base[2]), 255]
    }

    /// `frame` at the artwork size: as-is when it matches, otherwise scaled to
    /// fit and centred on transparency.
    fn fit<'a>(&self, frame: &'a Frame) -> Cow<'a, [u8]> {
        let (aw, ah) = (self.art_w, self.art_h);
        if frame.width == aw && frame.height == ah {
            return Cow::Borrowed(&frame.pixels);
        }
        #[allow(clippy::cast_precision_loss)]
        let scale = (aw as f32 / frame.width as f32).min(ah as f32 / frame.height as f32);
        let dw = ((frame.width as f32 * scale).round() as u32).clamp(1, aw);
        let dh = ((frame.height as f32 * scale).round() as u32).clamp(1, ah);
        let scaled = scale_bgra8_bilinear(&frame.pixels, frame.width, frame.height, dw, dh);
        let mut out = vec![0u8; aw as usize * ah as usize * 4];
        let (ox, oy) = ((aw - dw) / 2, (ah - dh) / 2);
        let row = dw as usize * 4;
        for y in 0..dh as usize {
            let dst = ((oy as usize + y) * aw as usize + ox as usize) * 4;
            out[dst..dst + row].copy_from_slice(&scaled[y * row..y * row + row]);
        }
        Cow::Owned(out)
    }
}

fn grow(side: u32) -> u32 {
    #[allow(clippy::cast_precision_loss)]
    let grown = (side as f32 * (1.0 + CANVAS_MARGIN)).round() as u32;
    grown.max(side)
}

struct Metadata {
    description: String,
    software: String,
    title: String,
    started_ms: u64,
}

impl Metadata {
    fn new(job: &ExportJob, scan: &Scan) -> Self {
        let canvas_w = scan.width << scan.halvings;
        let canvas_h = scan.height << scan.halvings;
        Self {
            description: format!(
                "Timelapse of {} - {} frames, canvas {canvas_w}x{canvas_h}, recorded {} to {} UTC",
                job.title,
                scan.frames,
                format_utc(scan.first_ms, '-'),
                format_utc(scan.last_ms, '-'),
            ),
            software: job.software.clone(),
            title: job.title.clone(),
            started_ms: scan.first_ms,
        }
    }
}

fn format_utc(ms: u64, date_sep: char) -> String {
    let [y, mo, d, h, mi, s] = crate::project::save::utc_fields(ms / 1000);
    format!("{y:04}{date_sep}{mo:02}{date_sep}{d:02} {h:02}:{mi:02}:{s:02}")
}

enum Sink {
    Video { child: Child, path: PathBuf, stderr: std::thread::JoinHandle<String> },
    Sequence { dir: PathBuf, prefix: String, written: Vec<PathBuf> },
}

impl Sink {
    fn video(
        ffmpeg: &Path,
        path: &Path,
        layout: &Layout,
        options: ExportOptions,
        meta: Option<&Metadata>,
    ) -> Result<Self, RecordingError> {
        let alpha = layout.has_alpha();
        if let Some(name) = missing_encoder(ffmpeg, options.format.encoders(alpha))? {
            return Err(RecordingError::Ffmpeg(format!(
                "this FFmpeg build has no {name} encoder, which {} needs",
                options.format.label()
            )));
        }
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-f", "rawvideo", "-pix_fmt"])
            .arg(if alpha { "rgba" } else { "rgb24" })
            .arg("-s")
            .arg(format!("{}x{}", layout.out_w, layout.out_h))
            .arg("-framerate")
            .arg(options.frame_rate.fps().to_string())
            .args(["-i", "-", "-an"])
            .args(codec_args(options, alpha))
            // SVT-AV1 logs its setup to stderr unless told to keep to errors.
            .env("SVT_LOG", "1");
        if let Some(meta) = meta {
            let started = format_utc(meta.started_ms, '-').replace(' ', "T");
            cmd.arg("-metadata").arg(format!("title={}", meta.title));
            cmd.arg("-metadata").arg(format!("comment={} ({})", meta.description, meta.software));
            cmd.arg("-metadata").arg(format!("creation_time={started}Z"));
        }
        cmd.arg(path).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| RecordingError::Ffmpeg(e.to_string()))?;
        // Drained on its own thread so a chatty ffmpeg can't block on a full pipe.
        let mut err_pipe = child.stderr.take().ok_or(RecordingError::Corrupt("ffmpeg stderr"))?;
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = err_pipe.read_to_string(&mut text);
            text
        });
        Ok(Self::Video { child, path: path.to_path_buf(), stderr })
    }

    fn write(
        &mut self,
        rgba: &[u8],
        layout: &Layout,
        compression: Compression,
        meta: Option<&Metadata>,
        time_ms: u64,
        index: u64,
    ) -> Result<(), RecordingError> {
        let pixels: Cow<'_, [u8]> = if layout.has_alpha() {
            Cow::Borrowed(rgba)
        } else {
            Cow::Owned(rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect())
        };
        match self {
            Self::Video { child, .. } => {
                let stdin = child.stdin.as_mut().ok_or(RecordingError::Corrupt("ffmpeg stdin"))?;
                // A broken pipe means ffmpeg quit; `finish` reports why.
                stdin.write_all(&pixels).map_err(|e| RecordingError::Ffmpeg(e.to_string()))
            }
            Self::Sequence { dir, prefix, written } => {
                let path = dir.join(format!("{prefix}_{index:05}.png"));
                written.push(path.clone());
                let file = BufWriter::new(std::fs::File::create(&path)?);
                let mut encoder = png::Encoder::new(file, layout.out_w, layout.out_h);
                encoder.set_color(if layout.has_alpha() { png::ColorType::Rgba } else { png::ColorType::Rgb });
                encoder.set_depth(png::BitDepth::Eight);
                encoder.set_compression(compression.png());
                let png_err = |e: png::EncodingError| RecordingError::Png(e.to_string());
                let mut writer = encoder.write_header().map_err(png_err)?;
                if let Some(meta) = meta {
                    let exif = exif_blob(&meta.description, &meta.software, &format_utc(time_ms, ':'));
                    writer.write_chunk(png::chunk::ChunkType(*b"eXIf"), &exif).map_err(png_err)?;
                }
                writer.write_image_data(&pixels).map_err(png_err)?;
                writer.finish().map_err(png_err)
            }
        }
    }

    fn finish(self) -> Result<(), RecordingError> {
        match self {
            Self::Video { mut child, path, stderr } => {
                drop(child.stdin.take());
                let status = child.wait()?;
                let message = stderr.join().unwrap_or_default();
                if status.success() {
                    Ok(())
                } else {
                    let _ = std::fs::remove_file(&path);
                    Err(RecordingError::Ffmpeg(last_line(&message, status)))
                }
            }
            Self::Sequence { .. } => Ok(()),
        }
    }

    fn abandon(self) {
        match self {
            Self::Video { mut child, path, .. } => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(path);
            }
            Self::Sequence { dir, written, .. } => {
                for path in written {
                    let _ = std::fs::remove_file(path);
                }
                let _ = std::fs::remove_dir(dir);
            }
        }
    }
}

// On a missing encoder ffmpeg only says "Encoder not found", without naming it.
fn missing_encoder(ffmpeg: &Path, names: &[&'static str]) -> Result<Option<&'static str>, RecordingError> {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| RecordingError::Ffmpeg(e.to_string()))?;
    let listing = String::from_utf8_lossy(&out.stdout);
    let has = |name: &str| listing.lines().any(|line| line.split_whitespace().nth(1) == Some(name));
    Ok(names.iter().copied().find(|name| !has(name)))
}

const BT709_TAGS: &str = "setparams=colorspace=bt709:color_primaries=bt709:color_trc=bt709";

// Players assume BT.709 for untagged HD video, so converting with ffmpeg's
// BT.601 default shifts the colours; convert with BT.709 and say so.
fn to_yuv(pix_fmt: &str) -> String {
    format!("scale=out_color_matrix=bt709:out_range=tv,format={pix_fmt},{BT709_TAGS}:range=tv")
}

/// Everything between the raw input and the output path. Each codec list ends
/// with the flag that takes the compression level.
fn codec_args(options: ExportOptions, alpha: bool) -> Vec<String> {
    let (filter_flag, filter) = match (options.format, alpha) {
        (OutputFormat::Avif, true) => (
            "-filter_complex",
            format!(
                "[0:v]split[c][a];[c]{}[c];[a]alphaextract,format=gray,{BT709_TAGS}:range=pc[a]",
                to_yuv("yuv420p")
            ),
        ),
        (OutputFormat::ProRes, true) => ("-vf", to_yuv("yuva444p10le")),
        (OutputFormat::ProRes, false) => ("-vf", to_yuv("yuv444p10le")),
        _ => ("-vf", to_yuv("yuv420p")),
    };
    let codec: &[&str] = match (options.format, alpha) {
        (OutputFormat::Avif, true) => &[
            "-map", "[c]", "-map", "[a]", "-c:v:0", "libsvtav1", "-preset:v:0", "8",
            "-c:v:1", "libaom-av1", "-cpu-used:v:1", "8", "-row-mt:v:1", "1", "-crf",
        ],
        (OutputFormat::Avif, false) => &["-c:v", "libsvtav1", "-preset", "8", "-crf"],
        (OutputFormat::ProRes, true) => {
            &["-c:v", "prores_ks", "-profile:v", "4444", "-vendor", "apl0", "-alpha_bits", "16", "-qscale:v"]
        }
        (OutputFormat::ProRes, false) => &["-c:v", "prores_ks", "-profile:v", "4444", "-vendor", "apl0", "-qscale:v"],
        (OutputFormat::H264 | OutputFormat::ImageSequence, _) => &["-c:v", "libx264", "-preset", "medium", "-crf"],
    };
    let mut args = vec![filter_flag.to_string(), filter];
    args.extend(codec.iter().map(|s| (*s).to_string()));
    args.push(options.compression.level(options.format).to_string());
    if options.format != OutputFormat::Avif {
        args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    }
    args
}

fn last_line(stderr: &str, status: std::process::ExitStatus) -> String {
    stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map_or_else(|| format!("exited with {status}"), str::to_string)
}

/// A minimal big-endian TIFF/EXIF block with ImageDescription, Software and
/// DateTime (`YYYY:MM:DD HH:MM:SS`).
fn exif_blob(description: &str, software: &str, datetime: &str) -> Vec<u8> {
    let fields: [(u16, &str); 3] = [(0x010E, description), (0x0131, software), (0x0132, datetime)];
    let count = fields.len() as u32;
    let data_start = 8 + 2 + count * 12 + 4;
    let mut out = b"MM\0\x2A".to_vec();
    out.extend_from_slice(&8u32.to_be_bytes());
    out.extend_from_slice(&(count as u16).to_be_bytes());
    let mut data: Vec<u8> = Vec::new();
    for (tag, text) in fields {
        let mut bytes: Vec<u8> = text.chars().map(|c| if c.is_ascii() { c as u8 } else { b'?' }).collect();
        bytes.push(0);
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&2u16.to_be_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        if bytes.len() <= 4 {
            let mut inline = [0u8; 4];
            inline[..bytes.len()].copy_from_slice(&bytes);
            out.extend_from_slice(&inline);
        } else {
            out.extend_from_slice(&(data_start + data.len() as u32).to_be_bytes());
            data.extend_from_slice(&bytes);
            if data.len() % 2 == 1 {
                data.push(0);
            }
        }
    }
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&data);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::recording::codec::{Encoded, FrameEncoder};

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oxiedraw_export_{}_{tag}", std::process::id()))
    }

    fn spool(tag: &str, frames: &[Frame]) -> SegmentSource {
        let mut enc = FrameEncoder::default();
        let mut bytes = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            if let Encoded::Record(r) = enc.encode(f.clone(), 0, 1_000 * (i as u64 + 1)).unwrap() {
                bytes.extend_from_slice(&r);
            }
        }
        let path = temp(tag);
        std::fs::write(&path, &bytes).unwrap();
        SegmentSource::new(std::fs::File::open(path).unwrap(), 0, bytes.len() as u64)
    }

    fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Frame {
        Frame { width, height, pixels: bgra.repeat((width * height) as usize) }
    }

    fn job(sources: Vec<SegmentSource>, options: ExportOptions, dir: PathBuf) -> ExportJob {
        ExportJob {
            sources,
            options,
            target: ExportTarget::Sequence { dir, prefix: "t".into() },
            margin_rgb: [31, 31, 36],
            title: "Test".into(),
            software: "OxieDraw test".into(),
        }
    }

    fn read_png(path: &Path) -> (png::OutputInfo, Vec<u8>) {
        let mut reader = png::Decoder::new(std::fs::File::open(path).unwrap()).read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        (info, buf)
    }

    #[test]
    fn sequence_writes_one_png_per_frame_over_the_background() {
        let source = spool("seq", &[solid(4, 2, [0, 0, 0, 0]), solid(4, 2, [0, 0, 255, 255])]);
        let dir = temp("seq_out");
        let options = ExportOptions { format: OutputFormat::ImageSequence, ..ExportOptions::default() };
        let progress = ExportProgress::default();
        assert_eq!(run(&job(vec![source], options, dir.clone()), &progress).unwrap(), 2);
        let (info, first) = read_png(&dir.join("t_00001.png"));
        assert_eq!((info.width, info.height), (4, 2));
        assert_eq!(&first[..3], &[255, 255, 255], "transparent frame shows the white background");
        let (_, second) = read_png(&dir.join("t_00002.png"));
        assert_eq!(&second[..3], &[255, 0, 0]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn include_canvas_grows_the_frame_by_a_fifth_in_the_margin_colour() {
        let source = spool("margin", &[solid(10, 5, [0, 0, 0, 255])]);
        let dir = temp("margin_out");
        let options = ExportOptions {
            format: OutputFormat::ImageSequence,
            include_canvas: true,
            ..ExportOptions::default()
        };
        run(&job(vec![source], options, dir.clone()), &ExportProgress::default()).unwrap();
        let (info, px) = read_png(&dir.join("t_00001.png"));
        assert_eq!((info.width, info.height), (12, 6));
        assert_eq!(&px[..3], &[31, 31, 36]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn alpha_background_keeps_transparency_in_the_sequence() {
        let source = spool("alpha", &[solid(2, 2, [0, 0, 0, 0])]);
        let dir = temp("alpha_out");
        let options = ExportOptions {
            format: OutputFormat::ImageSequence,
            background: Background::Alpha,
            ..ExportOptions::default()
        };
        run(&job(vec![source], options, dir.clone()), &ExportProgress::default()).unwrap();
        let (info, px) = read_png(&dir.join("t_00001.png"));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(px[3], 0);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn only_h264_drops_alpha() {
        let alpha = |format| ExportOptions { format, background: Background::Alpha, ..ExportOptions::default() };
        assert_eq!(alpha(OutputFormat::H264).effective_background(), Background::White);
        assert!(!Background::choices(OutputFormat::H264).contains(&Background::Alpha));
        for format in [OutputFormat::Avif, OutputFormat::ProRes, OutputFormat::ImageSequence] {
            assert!(alpha(format).has_alpha(), "{format:?} keeps alpha");
        }
    }

    #[test]
    fn avif_adds_an_alpha_plane_only_with_alpha() {
        let avif = ExportOptions { format: OutputFormat::Avif, ..ExportOptions::default() };
        assert_eq!(OutputFormat::Avif.extension(), "avif");
        assert_eq!(OutputFormat::Avif.encoders(false), ["libsvtav1"]);
        assert_eq!(OutputFormat::Avif.encoders(true), ["libsvtav1", "libaom-av1"]);
        assert!(!codec_args(avif, false).contains(&"-filter_complex".to_string()));
        let args = codec_args(ExportOptions { background: Background::Alpha, ..avif }, true);
        assert!(args.contains(&"-filter_complex".to_string()));
        assert!(!args.contains(&"-movflags".to_string()), "AVIF has no moov atom to move");
    }

    #[test]
    fn prores_keeps_alpha_bits_only_with_alpha() {
        let prores = ExportOptions { format: OutputFormat::ProRes, ..ExportOptions::default() };
        assert_eq!(OutputFormat::ProRes.extension(), "mov");
        assert!(codec_args(prores, true).contains(&"-alpha_bits".to_string()));
        assert!(!codec_args(prores, false).contains(&"-alpha_bits".to_string()));
    }

    #[test]
    fn smaller_earlier_frames_are_fitted_into_the_final_size() {
        let source = spool("fit", &[solid(2, 2, [0, 255, 0, 255]), solid(4, 2, [255, 0, 0, 255])]);
        let dir = temp("fit_out");
        let options = ExportOptions { format: OutputFormat::ImageSequence, ..ExportOptions::default() };
        run(&job(vec![source], options, dir.clone()), &ExportProgress::default()).unwrap();
        let (info, px) = read_png(&dir.join("t_00001.png"));
        assert_eq!((info.width, info.height), (4, 2));
        assert_eq!(&px[..3], &[255, 255, 255], "letterbox shows the background");
        assert_eq!(&px[3..6], &[0, 255, 0]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cancel_removes_what_was_written() {
        let source = spool("cancel", &[solid(2, 2, [1, 1, 1, 255])]);
        let dir = temp("cancel_out");
        let options = ExportOptions { format: OutputFormat::ImageSequence, ..ExportOptions::default() };
        let progress = ExportProgress::default();
        progress.cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            run(&job(vec![source], options, dir.clone()), &progress),
            Err(RecordingError::Cancelled)
        ));
        assert!(!dir.exists());
    }

    #[test]
    fn exif_block_is_a_well_formed_big_endian_ifd() {
        let blob = exif_blob("A description", "OxieDraw", "2026:09:18 10:00:00");
        assert_eq!(&blob[..4], b"MM\0\x2A");
        assert_eq!(u16::from_be_bytes([blob[8], blob[9]]), 3);
        let first_tag = u16::from_be_bytes([blob[10], blob[11]]);
        assert_eq!(first_tag, 0x010E);
        let offset = u32::from_be_bytes([blob[18], blob[19], blob[20], blob[21]]) as usize;
        assert_eq!(&blob[offset..offset + 13], b"A description");
    }

    #[test]
    fn empty_recording_is_an_error() {
        let path = temp("empty");
        std::fs::write(&path, b"").unwrap();
        let source = SegmentSource::new(std::fs::File::open(path).unwrap(), 0, 0);
        let options = ExportOptions { format: OutputFormat::ImageSequence, ..ExportOptions::default() };
        assert!(matches!(
            run(&job(vec![source], options, temp("empty_out")), &ExportProgress::default()),
            Err(RecordingError::Empty)
        ));
    }
}
