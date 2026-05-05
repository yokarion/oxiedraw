use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
    Avif,
}

impl ExportFormat {
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Avif => "avif",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Webp => "WebP",
            Self::Avif => "AVIF",
        }
    }

    pub const fn tags(self) -> &'static [&'static str] {
        match self {
            Self::Png => &["Lossless", "Transparency"],
            Self::Jpeg => &["Lossy"],
            Self::Webp => &["Lossy", "Transparency", "Web"],
            Self::Avif => &["Lossy", "Transparency"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PngBitDepth {
    #[default]
    Eight,
    Sixteen,
}

impl PngBitDepth {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Eight => "8-bit",
            Self::Sixteen => "16-bit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChromaSubsampling {
    Cs444,
    Cs422,
    #[default]
    Cs420,
    Cs411,
}

impl ChromaSubsampling {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cs444 => "4:4:4",
            Self::Cs422 => "4:2:2",
            Self::Cs420 => "4:2:0",
            Self::Cs411 => "4:1:1",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PngSettings {
    #[serde(default = "default_true")]
    pub transparency: bool,
    #[serde(default)]
    pub bit_depth: PngBitDepth,
    #[serde(default)]
    pub interlaced: bool,
    #[serde(default = "default_png_compression")]
    pub compression: u8,
}

impl Default for PngSettings {
    fn default() -> Self {
        Self {
            transparency: true,
            bit_depth: PngBitDepth::Eight,
            interlaced: false,
            compression: 6,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JpegSettings {
    #[serde(default = "default_jpeg_quality")]
    pub quality: u8,
    #[serde(default)]
    pub blur: f32,
    #[serde(default)]
    pub progressive: bool,
    #[serde(default)]
    pub chroma_subsampling: ChromaSubsampling,
}

impl Default for JpegSettings {
    fn default() -> Self {
        Self {
            quality: 85,
            blur: 0.0,
            progressive: false,
            chroma_subsampling: ChromaSubsampling::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebpSettings {
    #[serde(default)]
    pub lossless: bool,
    #[serde(default = "default_webp_quality")]
    pub quality: u8,
    #[serde(default = "default_true")]
    pub transparency: bool,
    #[serde(default = "default_webp_effort")]
    pub effort: u8,
}

impl Default for WebpSettings {
    fn default() -> Self {
        Self {
            lossless: false,
            quality: 80,
            transparency: true,
            effort: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvifSettings {
    #[serde(default)]
    pub lossless: bool,
    #[serde(default = "default_avif_quality")]
    pub quality: u8,
    #[serde(default = "default_true")]
    pub transparency: bool,
    #[serde(default = "default_avif_speed")]
    pub speed: u8,
}

impl Default for AvifSettings {
    fn default() -> Self {
        Self {
            lossless: false,
            quality: 60,
            transparency: true,
            speed: 6,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSettings {
    #[serde(default)]
    pub format: ExportFormat,
    #[serde(default = "default_scale")]
    pub scale: f32,
    #[serde(default)]
    pub png: PngSettings,
    #[serde(default)]
    pub jpeg: JpegSettings,
    #[serde(default)]
    pub webp: WebpSettings,
    #[serde(default)]
    pub avif: AvifSettings,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            format: ExportFormat::Png,
            scale: 1.0,
            png: PngSettings::default(),
            jpeg: JpegSettings::default(),
            webp: WebpSettings::default(),
            avif: AvifSettings::default(),
        }
    }
}

const fn default_true() -> bool {
    true
}
const fn default_png_compression() -> u8 {
    6
}
const fn default_jpeg_quality() -> u8 {
    85
}
const fn default_webp_quality() -> u8 {
    80
}
const fn default_webp_effort() -> u8 {
    4
}
const fn default_avif_quality() -> u8 {
    60
}
const fn default_avif_speed() -> u8 {
    6
}
const fn default_scale() -> f32 {
    1.0
}
