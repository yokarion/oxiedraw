//! Canvas timelapse recording.
//!
//! Frames are stored as tile deltas ([`codec`]) and appended to a spool file
//! by a background encoder ([`spool`]). Each save copies the frames added since
//! the previous save into the project as one more `recording/seg-NNNNN.bin`
//! entry, indexed by `recording.json` ([`segments`]); opening a project only
//! reads that index. [`export`] replays the frames into a video or PNG files.

pub mod codec;
pub mod export;
pub mod segments;
pub mod spool;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::enum_meta::EnumMeta;

pub const MANIFEST_ENTRY: &str = "recording.json";
pub const SEGMENT_DIR: &str = "recording/";

#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("PNG error: {0}")]
    Png(String),
    #[error("corrupt recording: {0}")]
    Corrupt(&'static str),
    #[error("ffmpeg failed: {0}")]
    Ffmpeg(String),
    #[error("nothing has been recorded yet")]
    Empty,
    #[error("export cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Frequency {
    EveryStroke,
    HalfSecond,
    #[default]
    OneSecond,
    FiveSeconds,
    TenSeconds,
    ThirtySeconds,
}

impl Frequency {
    /// Time between frames; `None` records one frame per finished change.
    #[must_use]
    pub const fn interval(self) -> Option<Duration> {
        match self {
            Self::EveryStroke => None,
            Self::HalfSecond => Some(Duration::from_millis(500)),
            Self::OneSecond => Some(Duration::from_secs(1)),
            Self::FiveSeconds => Some(Duration::from_secs(5)),
            Self::TenSeconds => Some(Duration::from_secs(10)),
            Self::ThirtySeconds => Some(Duration::from_secs(30)),
        }
    }
}

impl EnumMeta for Frequency {
    const ALL: &'static [Self] = &[
        Self::EveryStroke,
        Self::HalfSecond,
        Self::OneSecond,
        Self::FiveSeconds,
        Self::TenSeconds,
        Self::ThirtySeconds,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::EveryStroke => "Every stroke",
            Self::HalfSecond => "Every 0.5 seconds",
            Self::OneSecond => "Every 1 second",
            Self::FiveSeconds => "Every 5 seconds",
            Self::TenSeconds => "Every 10 seconds",
            Self::ThirtySeconds => "Every 30 seconds",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CanvasScale {
    Original,
    #[default]
    Half,
    Quarter,
}

impl CanvasScale {
    #[must_use]
    pub const fn halvings(self) -> u8 {
        match self {
            Self::Original => 0,
            Self::Half => 1,
            Self::Quarter => 2,
        }
    }
}

impl EnumMeta for CanvasScale {
    const ALL: &'static [Self] = &[Self::Original, Self::Half, Self::Quarter];

    fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Half => "0.5x",
            Self::Quarter => "0.25x",
        }
    }
}

/// Per-project recording settings, saved in `recording.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingSettings {
    #[serde(default)]
    pub frequency: Frequency,
    #[serde(default = "crate::serde_defaults::default_true")]
    pub skip_unchanged: bool,
    #[serde(default)]
    pub scale: CanvasScale,
    #[serde(default)]
    pub mid_stroke: bool,
    #[serde(default)]
    pub auto_start: bool,
}

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            frequency: Frequency::default(),
            skip_unchanged: true,
            scale: CanvasScale::default(),
            mid_stroke: false,
            auto_start: false,
        }
    }
}

/// One `recording/seg-NNNNN.bin` archive entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEntry {
    pub name: String,
    pub frames: u64,
    pub bytes: u64,
}

/// The contents of `recording.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingManifest {
    #[serde(default)]
    pub settings: RecordingSettings,
    #[serde(default)]
    pub segments: Vec<SegmentEntry>,
}

#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
