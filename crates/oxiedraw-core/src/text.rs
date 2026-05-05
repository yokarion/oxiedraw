//! Text layer content model.
//!
//! A text layer is a non-raster [`crate::document::LayerKind::Text`] whose
//! canvas-sized slot is re-rendered from this structured content (mirroring how
//! component instances render from a master). Styling is fully per-range: every
//! [`TextRun`] carries its own [`TextStyle`], so font, size, colour and the
//! bold/italic/underline flags can vary mid-string. Alignment and resize mode
//! are the only box-level properties.

use oxiedraw_utils::geometry::TransformRect;
use serde::{Deserialize, Serialize};

use crate::color::Color;

pub mod editor;
pub mod fonts;
pub mod render;

/// How a text box sizes itself relative to its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ResizeMode {
    /// Box hugs the text; width follows the longest line. No resize handles.
    AutoWidth,
    /// Fixed width, height grows with content. Width handles only.
    AutoHeight,
    /// Explicit width and height; content clips. All eight handles.
    #[default]
    Fixed,
}

/// Horizontal alignment of lines within the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// Vertical alignment of the text block within the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VAlign {
    #[default]
    Top,
    Middle,
    Bottom,
}

/// Reference to a font in the document's font registry. The string is the
/// font's family key (the registry maps it to embedded bytes, see Phase 2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FontId(pub String);

impl FontId {
    #[must_use]
    pub fn new(family: impl Into<String>) -> Self {
        Self(family.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Default font size in CSS-style pixels for freshly created text.
pub const DEFAULT_FONT_SIZE: f32 = 20.0;

/// Style applied to a run of characters. Fully self-contained so a run can be
/// split/merged without consulting box-level state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextStyle {
    pub font: FontId,
    /// Face/weight/style name within the family, e.g. "Regular" or "Bold".
    pub family_style: String,
    pub size: f32,
    pub color: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl TextStyle {
    /// A default style at [`DEFAULT_FONT_SIZE`] in the given colour and font.
    #[must_use]
    pub fn new(font: FontId, color: Color) -> Self {
        Self {
            font,
            family_style: "Regular".to_string(),
            size: DEFAULT_FONT_SIZE,
            color,
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

/// One contiguous run of characters sharing a single [`TextStyle`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextRun {
    pub text: String,
    pub style: TextStyle,
}

impl TextRun {
    #[must_use]
    pub fn new(text: impl Into<String>, style: TextStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// The text box rectangle on the canvas. Centre-based with a rotation angle,
/// mirroring [`TransformRect`] so the existing transform/handle machinery can
/// drive it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TextBox {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub angle: f32,
}

impl TextBox {
    #[must_use]
    pub const fn new(cx: f32, cy: f32, w: f32, h: f32, angle: f32) -> Self {
        Self {
            cx,
            cy,
            w,
            h,
            angle,
        }
    }

    #[must_use]
    pub const fn to_rect(self) -> TransformRect {
        TransformRect::new(self.cx, self.cy, self.w, self.h, self.angle)
    }

    #[must_use]
    pub const fn from_rect(r: TransformRect) -> Self {
        Self {
            cx: r.cx,
            cy: r.cy,
            w: r.w,
            h: r.h,
            angle: r.angle,
        }
    }
}

/// Default (identity) anamorphic scale for [`TextContent::scale`].
fn default_scale() -> (f32, f32) {
    (1.0, 1.0)
}

/// Structured content of a text layer: the source of truth that the slot
/// pixels are rendered from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextContent {
    /// The text box in *natural* (unscaled) layout coordinates: `w`/`h` are the
    /// wrap width / block height the glyphs are laid out at; `cx`/`cy`/`angle`
    /// place it on the canvas. The on-screen box is this scaled by [`Self::scale`].
    pub box_rect: TextBox,
    pub resize: ResizeMode,
    pub h_align: HAlign,
    pub v_align: VAlign,
    /// Character runs in document order. May be empty (a fresh, empty box).
    pub runs: Vec<TextRun>,
    /// Style applied to newly typed characters when no run dictates one (empty
    /// box, or typing at a boundary).
    pub default_style: TextStyle,
    /// Anamorphic display scale `(sx, sy)` applied on top of the natural layout
    /// (set by the Transform tool's scale). `(1.0, 1.0)` = no squish. The text
    /// stays editable at its natural size; the squish is applied when rendering
    /// and when mapping editor coordinates.
    #[serde(default = "default_scale")]
    pub scale: (f32, f32),
}

impl TextContent {
    /// An empty box at `box_rect` in the given resize mode, carrying `style`
    /// as both the (absent) content style and the default for new typing.
    #[must_use]
    pub fn empty(box_rect: TextBox, resize: ResizeMode, style: TextStyle) -> Self {
        Self {
            box_rect,
            resize,
            h_align: HAlign::default(),
            v_align: VAlign::default(),
            runs: Vec::new(),
            default_style: style,
            scale: default_scale(),
        }
    }

    /// `true` when an anamorphic squish is applied (scale differs from identity).
    #[must_use]
    pub fn is_scaled(&self) -> bool {
        (self.scale.0 - 1.0).abs() > 1e-3 || (self.scale.1 - 1.0).abs() > 1e-3
    }

    /// The on-screen box: the natural [`box_rect`](Self::box_rect) with its
    /// width/height scaled by [`scale`](Self::scale). Centre and angle are
    /// unchanged. This is what the user sees and interacts with.
    #[must_use]
    pub fn visible_rect(&self) -> TransformRect {
        let b = self.box_rect;
        TransformRect::new(b.cx, b.cy, b.w * self.scale.0, b.h * self.scale.1, b.angle)
    }

    /// Concatenated plain text across all runs.
    #[must_use]
    pub fn plain_text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// Total character count across all runs.
    #[must_use]
    pub fn char_len(&self) -> usize {
        self.runs.iter().map(|r| r.text.chars().count()).sum()
    }

    /// `true` when there is no typed text (handles never-typed and all-deleted).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.runs.iter().all(|r| r.text.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> TextStyle {
        TextStyle::new(FontId::new("Inter"), Color::BLACK)
    }

    #[test]
    fn box_rect_roundtrip() {
        let b = TextBox::new(10.0, 20.0, 30.0, 40.0, 0.5);
        assert_eq!(b, TextBox::from_rect(b.to_rect()));
    }

    #[test]
    fn plain_text_and_len_concatenate_runs() {
        let content = TextContent {
            box_rect: TextBox::new(0.0, 0.0, 100.0, 40.0, 0.0),
            resize: ResizeMode::Fixed,
            h_align: HAlign::Left,
            v_align: VAlign::Top,
            runs: vec![
                TextRun::new("He", style()),
                TextRun::new("llo", style()),
            ],
            default_style: style(),
            scale: (1.0, 1.0),
        };
        assert_eq!(content.plain_text(), "Hello");
        assert_eq!(content.char_len(), 5);
        assert!(!content.is_empty());
    }

    #[test]
    fn scale_defaults_to_identity_and_visible_rect_scales() {
        let mut c = TextContent::empty(
            TextBox::new(100.0, 50.0, 200.0, 40.0, 0.0),
            ResizeMode::Fixed,
            style(),
        );
        assert_eq!(c.scale, (1.0, 1.0));
        assert!(!c.is_scaled());
        // Identity scale: visible == natural.
        let v = c.visible_rect();
        assert!((v.w - 200.0).abs() < 1e-3 && (v.h - 40.0).abs() < 1e-3);

        // Squish width to half: visible width halves, centre/angle unchanged.
        c.scale = (0.5, 1.0);
        assert!(c.is_scaled());
        let v = c.visible_rect();
        assert!((v.w - 100.0).abs() < 1e-3, "w {}", v.w);
        assert!((v.h - 40.0).abs() < 1e-3);
        assert!((v.cx - 100.0).abs() < 1e-3 && (v.cy - 50.0).abs() < 1e-3);
    }

    #[test]
    fn visible_rect_preserves_centre_and_angle() {
        let mut c = TextContent::empty(
            TextBox::new(10.0, 20.0, 80.0, 30.0, 0.7),
            ResizeMode::Fixed,
            style(),
        );
        c.scale = (2.0, 0.5);
        assert!(c.is_scaled());
        let v = c.visible_rect();
        // Centre + angle unchanged; width/height scaled independently.
        assert!((v.cx - 10.0).abs() < 1e-3 && (v.cy - 20.0).abs() < 1e-3);
        assert!((v.angle - 0.7).abs() < 1e-3);
        assert!((v.w - 160.0).abs() < 1e-3, "w {}", v.w);
        assert!((v.h - 15.0).abs() < 1e-3, "h {}", v.h);
    }

    #[test]
    fn empty_box_reports_empty() {
        let c = TextContent::empty(
            TextBox::new(0.0, 0.0, 10.0, 10.0, 0.0),
            ResizeMode::AutoWidth,
            style(),
        );
        assert!(c.is_empty());
        assert_eq!(c.char_len(), 0);
        assert_eq!(c.plain_text(), "");
    }
}
