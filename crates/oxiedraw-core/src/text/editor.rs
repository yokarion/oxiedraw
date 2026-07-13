//! Live text editing state.
//!
//! [`TextEditor`] wraps cosmic-text's [`Editor`] (which handles caret, selection
//! and motion correctly, including wrapping) while keeping our box geometry and
//! resize behaviour. It exposes editing operations, caret/selection geometry in
//! box-local pixels (for the canvas overlay), slot rendering, and conversion
//! back to a [`TextContent`] on commit.

use cosmic_text::{
    Action, Align, Attrs, Cursor, Edit, Editor, Family, Metrics, Motion, Selection, Style, Weight,
    Wrap,
};

use super::fonts::TextEngine;
use super::render;
use super::{FontId, HAlign, ResizeMode, TextBox, TextContent, TextRun, TextStyle, VAlign};
use crate::color::Color;

/// Caret width in canvas pixels (the UI scales it by zoom).
pub const CARET_WIDTH: f32 = 1.5;

/// A box-local rectangle `(x, y, w, h)` with origin at the box's top-left.
pub type LocalRect = (f32, f32, f32, f32);

pub struct TextEditor {
    inner: Editor<'static>,
    box_rect: TextBox,
    resize: ResizeMode,
    h_align: HAlign,
    v_align: VAlign,
    default_style: TextStyle,
    /// Anamorphic display scale carried through editing (never affects layout;
    /// the editor works in natural coordinates). Echoed back by `to_content`.
    scale: (f32, f32),
}

impl TextEditor {
    /// Start editing the given content. The caret is placed at the end.
    #[must_use]
    pub fn from_content(content: &TextContent, engine: &mut TextEngine) -> Self {
        let buffer = render::shape(content, engine, render::wrap_width_for(content));
        let mut inner = Editor::new(buffer);
        inner.set_selection(Selection::None);
        inner.action(&mut engine.font_system, Action::Motion(Motion::BufferEnd));
        Self {
            inner,
            box_rect: content.box_rect,
            resize: content.resize,
            h_align: content.h_align,
            v_align: content.v_align,
            default_style: content.default_style.clone(),
            scale: content.scale,
        }
    }

    #[must_use]
    pub fn box_rect(&self) -> TextBox {
        self.box_rect
    }

    /// The anamorphic display scale `(sx, sy)` carried with this content.
    #[must_use]
    pub fn scale(&self) -> (f32, f32) {
        self.scale
    }

    /// Set the anamorphic display scale (Transform-tool commit). Does not
    /// reflow - the natural layout is unchanged; only the squish differs.
    pub fn set_scale(&mut self, scale: (f32, f32)) {
        self.scale = scale;
    }

    #[must_use]
    pub fn resize_mode(&self) -> ResizeMode {
        self.resize
    }

    /// Replace the box geometry (handle resize) and reflow.
    pub fn set_box(&mut self, box_rect: TextBox, engine: &mut TextEngine) {
        self.box_rect = box_rect;
        self.sync_layout(engine);
        self.auto_grow();
    }

    pub fn set_resize_mode(&mut self, resize: ResizeMode, engine: &mut TextEngine) {
        self.resize = resize;
        self.sync_layout(engine);
        self.auto_grow();
    }

    pub fn set_h_align(&mut self, h_align: HAlign, engine: &mut TextEngine) {
        self.h_align = h_align;
        self.sync_layout(engine);
    }

    pub fn set_v_align(&mut self, v_align: VAlign) {
        self.v_align = v_align;
    }

    // -- editing -----------------------------------------------------------

    pub fn insert_char(&mut self, engine: &mut TextEngine, c: char) {
        self.inner.action(&mut engine.font_system, Action::Insert(c));
        self.after_edit(engine);
    }

    pub fn insert_str(&mut self, engine: &mut TextEngine, s: &str) {
        self.inner.delete_selection();
        self.inner.insert_string(s, None);
        self.after_edit(engine);
    }

    pub fn enter(&mut self, engine: &mut TextEngine) {
        self.inner.action(&mut engine.font_system, Action::Enter);
        self.after_edit(engine);
    }

    pub fn backspace(&mut self, engine: &mut TextEngine) {
        self.inner.action(&mut engine.font_system, Action::Backspace);
        self.after_edit(engine);
    }

    pub fn delete(&mut self, engine: &mut TextEngine) {
        self.inner.action(&mut engine.font_system, Action::Delete);
        self.after_edit(engine);
    }

    pub fn move_left(&mut self, engine: &mut TextEngine, select: bool) {
        self.motion(engine, Motion::Left, select);
    }
    pub fn move_right(&mut self, engine: &mut TextEngine, select: bool) {
        self.motion(engine, Motion::Right, select);
    }
    pub fn move_up(&mut self, engine: &mut TextEngine, select: bool) {
        self.motion(engine, Motion::Up, select);
    }
    pub fn move_down(&mut self, engine: &mut TextEngine, select: bool) {
        self.motion(engine, Motion::Down, select);
    }
    pub fn move_home(&mut self, engine: &mut TextEngine, select: bool) {
        self.motion(engine, Motion::Home, select);
    }
    pub fn move_end(&mut self, engine: &mut TextEngine, select: bool) {
        self.motion(engine, Motion::End, select);
    }
    pub fn move_word_left(&mut self, engine: &mut TextEngine, select: bool) {
        self.motion(engine, Motion::LeftWord, select);
    }
    pub fn move_word_right(&mut self, engine: &mut TextEngine, select: bool) {
        self.motion(engine, Motion::RightWord, select);
    }

    /// Move the caret. `select` extends the selection from its anchor.
    fn motion(&mut self, engine: &mut TextEngine, motion: Motion, select: bool) {
        if select {
            if matches!(self.inner.selection(), Selection::None) {
                self.inner.set_selection(Selection::Normal(self.inner.cursor()));
            }
        } else {
            self.inner.set_selection(Selection::None);
        }
        self.inner.action(&mut engine.font_system, Action::Motion(motion));
        self.sync_layout(engine);
    }

    pub fn select_all(&mut self, engine: &mut TextEngine) {
        self.inner.set_cursor(Cursor::new(0, 0));
        self.inner.set_selection(Selection::Normal(Cursor::new(0, 0)));
        self.inner
            .action(&mut engine.font_system, Action::Motion(Motion::BufferEnd));
        self.sync_layout(engine);
    }

    // -- styling (selection or whole box) ----------------------------------

    /// Toggle bold over the selection, or the whole box when nothing is
    /// selected. Mixed -> all bold; uniformly bold -> not bold.
    pub fn toggle_bold(&mut self, engine: &mut TextEngine) {
        let on = !self.range_all(|s| s.bold);
        self.restyle(engine, |s| s.bold = on);
    }

    pub fn toggle_italic(&mut self, engine: &mut TextEngine) {
        let on = !self.range_all(|s| s.italic);
        self.restyle(engine, |s| s.italic = on);
    }

    pub fn toggle_underline(&mut self, engine: &mut TextEngine) {
        let on = !self.range_all(|s| s.underline);
        self.restyle(engine, |s| s.underline = on);
    }

    /// Set the colour over the selection, or the whole box when nothing is
    /// selected.
    pub fn set_color(&mut self, engine: &mut TextEngine, color: Color) {
        self.restyle(engine, move |s| s.color = color);
    }

    /// Set the font family over the selection, or the whole box.
    pub fn set_font(&mut self, engine: &mut TextEngine, font: FontId) {
        self.restyle(engine, move |s| s.font = font.clone());
    }

    /// Set the font size over the selection, or the whole box.
    pub fn set_size(&mut self, engine: &mut TextEngine, size: f32) {
        let size = size.max(1.0);
        self.restyle(engine, move |s| s.size = size);
    }

    /// Set the face (bold/italic) over the selection, or the whole box.
    pub fn set_face(&mut self, engine: &mut TextEngine, bold: bool, italic: bool) {
        self.restyle(engine, move |s| {
            s.bold = bold;
            s.italic = italic;
        });
    }

    /// The style at the caret (or selection start), for the properties panel.
    #[must_use]
    pub fn current_style(&self) -> TextStyle {
        let content = self.to_content();
        let target = self
            .selection_char_range()
            .map_or_else(|| self.cursor_char(), |(lo, _)| lo);
        let mut pos = 0;
        for run in &content.runs {
            let len = run.text.chars().count();
            if target < pos + len {
                return run.style.clone();
            }
            pos += len;
        }
        content.default_style
    }

    #[must_use]
    pub fn h_align(&self) -> HAlign {
        self.h_align
    }

    #[must_use]
    pub fn v_align(&self) -> VAlign {
        self.v_align
    }

    /// `true` if every (non-newline) run in the target range satisfies `pred`.
    fn range_all(&self, pred: impl Fn(&TextStyle) -> bool) -> bool {
        let content = self.to_content();
        let (lo, hi) = self
            .selection_char_range()
            .unwrap_or_else(|| (0, content.char_len()));
        let mut pos = 0;
        let mut any = false;
        for run in &content.runs {
            let len = run.text.chars().count();
            let (s, e) = (pos, pos + len);
            pos = e;
            if e <= lo || s >= hi || run.text == "\n" {
                continue;
            }
            any = true;
            if !pred(&run.style) {
                return false;
            }
        }
        any
    }

    /// Apply `f` to the styles in the target range (selection, or whole box),
    /// rebuild, and restore the caret/selection. When nothing is selected the
    /// default (new-typing) style is updated too.
    fn restyle(&mut self, engine: &mut TextEngine, f: impl Fn(&mut TextStyle)) {
        let sel = self.selection_char_range();
        let cursor_char = self.cursor_char();
        let mut content = self.to_content();
        let (lo, hi) = sel.unwrap_or_else(|| (0, content.char_len()));
        apply_style_range(&mut content.runs, lo, hi, &f);
        if sel.is_none() {
            f(&mut content.default_style);
        }
        self.rebuild(&content, engine);
        self.restore_cursor_selection(engine, cursor_char, sel);
        self.auto_grow();
    }

    /// Place the caret at a box-local point (origin = box top-left).
    pub fn click(&mut self, engine: &mut TextEngine, local_x: f32, local_y: f32) {
        let (x, y) = self.buffer_point(local_x, local_y);
        self.inner
            .action(&mut engine.font_system, Action::Click { x, y });
        self.sync_layout(engine);
    }

    /// Extend a drag-selection to a box-local point.
    pub fn drag(&mut self, engine: &mut TextEngine, local_x: f32, local_y: f32) {
        let (x, y) = self.buffer_point(local_x, local_y);
        self.inner
            .action(&mut engine.font_system, Action::Drag { x, y });
        self.sync_layout(engine);
    }

    #[must_use]
    pub fn copy(&self) -> Option<String> {
        self.inner.copy_selection()
    }

    pub fn cut(&mut self, engine: &mut TextEngine) -> Option<String> {
        let s = self.inner.copy_selection();
        if s.is_some() {
            self.inner.delete_selection();
            self.after_edit(engine);
        }
        s
    }

    #[must_use]
    pub fn has_selection(&self) -> bool {
        !matches!(self.inner.selection(), Selection::None)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner
            .with_buffer(|b| b.lines.len() <= 1 && b.lines.iter().all(|l| l.text().is_empty()))
    }

    // -- geometry / output -------------------------------------------------

    /// Caret rectangle in box-local pixels, or `None` if off-screen.
    pub fn caret_rect(&mut self, engine: &mut TextEngine) -> Option<LocalRect> {
        self.sync_layout(engine);
        let (cx, cy) = self.inner.cursor_position()?;
        let top = self.vertical_offset();
        let h = self.cursor_line_height();
        Some((cx as f32, cy as f32 + top, CARET_WIDTH, h))
    }

    /// Selection highlight rectangles in box-local pixels.
    pub fn selection_rects(&mut self, engine: &mut TextEngine) -> Vec<LocalRect> {
        self.sync_layout(engine);
        let Some((start, end)) = self.inner.selection_bounds() else {
            return Vec::new();
        };
        let top = self.vertical_offset();
        let mut rects = Vec::new();
        self.inner.with_buffer(|buffer| {
            for run in buffer.layout_runs() {
                if let Some((x, w)) = run.highlight(start, end) {
                    rects.push((x, run.line_top + top, w.max(1.0), run.line_height));
                }
            }
        });
        rects
    }

    /// Natural (width, height) of the text block.
    pub fn content_size(&mut self, engine: &mut TextEngine) -> (f32, f32) {
        self.sync_layout(engine);
        self.inner.with_buffer(render::block_size)
    }

    /// Render the current text into a canvas-sized BGRA8 slot (glyphs only; the
    /// caret/selection are drawn by the UI overlay, not baked into the layer).
    #[must_use]
    pub fn render_into_slot(
        &mut self,
        engine: &mut TextEngine,
        canvas_w: u32,
        canvas_h: u32,
    ) -> Vec<u8> {
        self.sync_layout(engine);
        if self.is_empty() {
            return vec![0u8; (canvas_w as usize) * (canvas_h as usize) * 4];
        }
        let (box_rect, scale, resize, v_align, color) = (
            self.box_rect,
            self.scale,
            self.resize,
            self.v_align,
            self.default_style.color,
        );
        self.inner.with_buffer(|buffer| {
            render::paint_buffer_scaled(
                buffer, box_rect, scale, resize, v_align, color, engine, canvas_w, canvas_h,
            )
        })
    }

    /// Render the text into a `region_w x region_h` BGRA8 buffer whose origin is
    /// canvas pixel `(region_x, region_y)` - i.e. only the dirty rectangle, with
    /// the box shifted into region space. Far cheaper than rendering the whole
    /// canvas slot per keystroke. The caller uploads it via
    /// `Canvas::restore_layer_region`.
    #[must_use]
    pub fn render_region(
        &mut self,
        engine: &mut TextEngine,
        region_x: i32,
        region_y: i32,
        region_w: u32,
        region_h: u32,
    ) -> Vec<u8> {
        self.sync_layout(engine);
        let len = (region_w as usize) * (region_h as usize) * 4;
        if self.is_empty() {
            return vec![0u8; len];
        }
        // Shift the box into region-local space and render as if the region were
        // a mini-canvas; the glyphs land at the right spot within the region.
        #[allow(clippy::cast_precision_loss)]
        let mut box_rect = self.box_rect;
        box_rect.cx -= region_x as f32;
        box_rect.cy -= region_y as f32;
        let (scale, resize, v_align, color) =
            (self.scale, self.resize, self.v_align, self.default_style.color);
        self.inner.with_buffer(|buffer| {
            render::paint_buffer_scaled(
                buffer, box_rect, scale, resize, v_align, color, engine, region_w, region_h,
            )
        })
    }

    /// Reconstruct a [`TextContent`] from the edited buffer (for commit/history).
    #[must_use]
    pub fn to_content(&self) -> TextContent {
        let mut runs: Vec<TextRun> = Vec::new();
        self.inner.with_buffer(|buffer| {
            let n = buffer.lines.len();
            for (i, line) in buffer.lines.iter().enumerate() {
                let text = line.text();
                let attrs_list = line.attrs_list();
                // `spans()` doesn't tile gaps (inserted text falls back to the
                // line defaults), so group chars by their effective attrs.
                if !text.is_empty() {
                    let mut seg_start = 0usize;
                    let mut seg_attrs = attrs_list.get_span(0);
                    for (byte, _) in text.char_indices() {
                        if byte == 0 {
                            continue;
                        }
                        let attrs = attrs_list.get_span(byte);
                        if attrs != seg_attrs {
                            runs.push(TextRun::new(
                                text[seg_start..byte].to_string(),
                                style_from_attrs(seg_attrs, &self.default_style),
                            ));
                            seg_start = byte;
                            seg_attrs = attrs;
                        }
                    }
                    runs.push(TextRun::new(
                        text[seg_start..].to_string(),
                        style_from_attrs(seg_attrs, &self.default_style),
                    ));
                }
                if i + 1 < n {
                    let nl_style = runs
                        .last()
                        .map_or_else(|| self.default_style.clone(), |r| r.style.clone());
                    runs.push(TextRun::new("\n".to_string(), nl_style));
                }
            }
        });
        TextContent {
            box_rect: self.box_rect,
            resize: self.resize,
            h_align: self.h_align,
            v_align: self.v_align,
            runs: merge_runs(runs),
            default_style: self.default_style.clone(),
            scale: self.scale,
        }
    }

    // -- internals ---------------------------------------------------------

    fn after_edit(&mut self, engine: &mut TextEngine) {
        self.sync_layout(engine);
        self.auto_grow();
    }

    /// Re-apply box size, wrap mode and alignment to the editor's buffer.
    fn sync_layout(&mut self, engine: &mut TextEngine) {
        let wrap_width = match self.resize {
            ResizeMode::AutoWidth => None,
            ResizeMode::AutoHeight | ResizeMode::Fixed => Some(self.box_rect.w.max(1.0)),
        };
        let align = ct_align(self.h_align);
        let fs = &mut engine.font_system;
        self.inner.with_buffer_mut(|buffer| {
            let wrap = if wrap_width.is_some() {
                Wrap::WordOrGlyph
            } else {
                Wrap::None
            };
            buffer.set_wrap(fs, wrap);
            buffer.set_size(fs, wrap_width, None);
            for line in &mut buffer.lines {
                line.set_align(Some(align));
            }
        });
        self.inner.shape_as_needed(&mut engine.font_system, false);
    }

    /// For auto modes, grow the box to fit content, keeping the *visible* top-left
    /// fixed: the natural box is squished about its centre by `scale`, so anchoring
    /// the natural corner would drift the on-screen origin under an anamorphic
    /// squish (e.g. after a vertical Transform stretch).
    fn auto_grow(&mut self) {
        let (bw, bh) = self.inner.with_buffer(render::block_size);
        let (sx, sy) = self.scale;
        let vis_left = self.box_rect.cx - self.box_rect.w * sx / 2.0;
        let vis_top = self.box_rect.cy - self.box_rect.h * sy / 2.0;
        let angle = self.box_rect.angle;
        match self.resize {
            ResizeMode::AutoWidth => {
                let w = bw.max(1.0);
                let h = bh.max(1.0);
                let cx = vis_left + w * sx / 2.0;
                let cy = vis_top + h * sy / 2.0;
                self.box_rect = TextBox::new(cx, cy, w, h, angle);
            }
            ResizeMode::AutoHeight => {
                let h = bh.max(1.0);
                let cy = vis_top + h * sy / 2.0;
                self.box_rect = TextBox::new(self.box_rect.cx, cy, self.box_rect.w, h, angle);
            }
            ResizeMode::Fixed => {}
        }
    }

    fn vertical_offset(&self) -> f32 {
        let (_, bh) = self.inner.with_buffer(render::block_size);
        render::vertical_offset(self.v_align, self.box_rect.h, bh)
    }

    /// Convert a box-local point to the editor buffer's coordinate space.
    fn buffer_point(&self, local_x: f32, local_y: f32) -> (i32, i32) {
        let top = self.vertical_offset();
        (local_x.round() as i32, (local_y - top).round() as i32)
    }

    fn cursor_line_height(&self) -> f32 {
        let line = self.inner.cursor().line;
        self.inner.with_buffer(|b| {
            b.layout_runs()
                .find(|r| r.line_i == line)
                .or_else(|| b.layout_runs().next())
                .map_or(self.default_style.size * 1.2, |r| r.line_height)
        })
    }

    /// Flat char range of the current selection, or `None` if nothing is
    /// selected.
    fn selection_char_range(&self) -> Option<(usize, usize)> {
        let (start, end) = self.inner.selection_bounds()?;
        self.inner.with_buffer(|b| {
            let a = cursor_to_char(b, start);
            let z = cursor_to_char(b, end);
            Some((a.min(z), a.max(z)))
        })
    }

    /// Flat char index of the caret.
    fn cursor_char(&self) -> usize {
        let c = self.inner.cursor();
        self.inner.with_buffer(|b| cursor_to_char(b, c))
    }

    /// Rebuild the cosmic-text editor from new content (after a restyle),
    /// keeping the box geometry.
    fn rebuild(&mut self, content: &TextContent, engine: &mut TextEngine) {
        let buffer = render::shape(content, engine, render::wrap_width_for(content));
        self.inner = Editor::new(buffer);
        self.default_style = content.default_style.clone();
        self.sync_layout(engine);
    }

    /// Restore the caret (and selection, if any) by flat char index.
    fn restore_cursor_selection(
        &mut self,
        engine: &mut TextEngine,
        cursor_char: usize,
        sel: Option<(usize, usize)>,
    ) {
        if let Some((lo, hi)) = sel {
            let anchor = self.inner.with_buffer(|b| char_to_cursor(b, lo));
            let head = self.inner.with_buffer(|b| char_to_cursor(b, hi));
            self.inner.set_selection(Selection::Normal(anchor));
            self.inner.set_cursor(head);
        } else {
            let cur = self.inner.with_buffer(|b| char_to_cursor(b, cursor_char));
            self.inner.set_selection(Selection::None);
            self.inner.set_cursor(cur);
        }
        self.sync_layout(engine);
    }
}

fn ct_align(h_align: HAlign) -> Align {
    match h_align {
        HAlign::Left => Align::Left,
        HAlign::Center => Align::Center,
        HAlign::Right => Align::Right,
    }
}

/// Map cosmic-text attributes back to one of our styles.
fn style_from_attrs(attrs: Attrs, default: &TextStyle) -> TextStyle {
    let font = match attrs.family {
        Family::Name(name) => FontId::new(name.to_string()),
        _ => default.font.clone(),
    };
    let size = attrs
        .metrics_opt
        .map_or(default.size, |m| Metrics::from(m).font_size);
    let color = attrs
        .color_opt
        .map_or(default.color, |c| Color::new(c.r(), c.g(), c.b()));
    TextStyle {
        font,
        family_style: default.family_style.clone(),
        size,
        color,
        bold: attrs.weight.0 >= Weight::BOLD.0,
        italic: matches!(attrs.style, Style::Italic | Style::Oblique),
        underline: attrs.metadata & 1 == 1,
    }
}

/// Apply `f` to every run-slice overlapping `[lo, hi)` (flat char indices over
/// the concatenated text, newlines counted), splitting runs at the boundaries
/// and merging equal neighbours afterwards.
fn apply_style_range(
    runs: &mut Vec<TextRun>,
    lo: usize,
    hi: usize,
    f: &impl Fn(&mut TextStyle),
) {
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len() + 2);
    let mut pos = 0;
    for run in runs.drain(..) {
        let chars: Vec<char> = run.text.chars().collect();
        let len = chars.len();
        let (s, e) = (pos, pos + len);
        pos = e;
        if e <= lo || s >= hi || len == 0 {
            out.push(run);
            continue;
        }
        let a = lo.saturating_sub(s).min(len); // local start of the styled span
        let b = hi.saturating_sub(s).min(len); // local end
        if a > 0 {
            out.push(TextRun::new(chars[..a].iter().collect::<String>(), run.style.clone()));
        }
        if b > a {
            let mut st = run.style.clone();
            f(&mut st);
            out.push(TextRun::new(chars[a..b].iter().collect::<String>(), st));
        }
        if b < len {
            out.push(TextRun::new(chars[b..].iter().collect::<String>(), run.style.clone()));
        }
    }
    *runs = merge_runs(out);
}

/// Flat char index of a buffer cursor (newlines between lines counted as one).
fn cursor_to_char(buffer: &cosmic_text::Buffer, cursor: Cursor) -> usize {
    let mut total = 0;
    for line in buffer.lines.iter().take(cursor.line) {
        total += line.text().chars().count() + 1;
    }
    if let Some(line) = buffer.lines.get(cursor.line) {
        let byte = cursor.index.min(line.text().len());
        total += line.text()[..byte].chars().count();
    }
    total
}

/// Inverse of [`cursor_to_char`].
fn char_to_cursor(buffer: &cosmic_text::Buffer, flat: usize) -> Cursor {
    let mut remaining = flat;
    for (li, line) in buffer.lines.iter().enumerate() {
        let line_chars = line.text().chars().count();
        if remaining <= line_chars {
            let byte = line
                .text()
                .char_indices()
                .nth(remaining)
                .map_or_else(|| line.text().len(), |(b, _)| b);
            return Cursor::new(li, byte);
        }
        remaining -= line_chars + 1;
    }
    let li = buffer.lines.len().saturating_sub(1);
    Cursor::new(li, buffer.lines.get(li).map_or(0, |l| l.text().len()))
}

/// Merge consecutive runs that share an identical style.
fn merge_runs(runs: Vec<TextRun>) -> Vec<TextRun> {
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len());
    for run in runs {
        if let Some(last) = out.last_mut()
            && last.style == run.style
        {
            last.text.push_str(&run.text);
        } else {
            out.push(run);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::fonts::TextEngine;
    use crate::text::TextBox;

    fn engine() -> TextEngine {
        TextEngine::new()
    }

    fn base_style(eng: &TextEngine) -> TextStyle {
        let fam = eng.available_families().into_iter().next().unwrap_or_else(|| "sans-serif".into());
        TextStyle::new(FontId::new(fam), Color::BLACK)
    }

    fn empty_editor(eng: &mut TextEngine, mode: ResizeMode) -> TextEditor {
        let style = base_style(eng);
        let content = TextContent::empty(TextBox::new(100.0, 50.0, 200.0, 40.0, 0.0), mode, style);
        TextEditor::from_content(&content, eng)
    }

    #[test]
    fn typing_builds_text() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let mut ed = empty_editor(&mut eng, ResizeMode::Fixed);
        assert!(ed.is_empty());
        for c in "Hello".chars() {
            ed.insert_char(&mut eng, c);
        }
        assert!(!ed.is_empty());
        assert_eq!(ed.to_content().plain_text(), "Hello");
    }

    #[test]
    fn scale_passthrough_and_render_squishes() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let mut ed = empty_editor(&mut eng, ResizeMode::Fixed);
        ed.insert_str(&mut eng, "Hello");

        // Scale travels through editing into to_content (passthrough).
        ed.set_scale((0.5, 1.0));
        assert_eq!(ed.scale(), (0.5, 1.0));
        assert_eq!(ed.to_content().scale, (0.5, 1.0));

        // The rendered slot must reflect the squish: rightmost opaque pixel
        // stays within the visible (half-width) box, well left of where the
        // un-squished glyphs would reach.
        let (cw, ch) = (256u32, 128u32);
        let px = ed.render_into_slot(&mut eng, cw, ch);
        let max_x = (0..ch)
            .flat_map(|y| (0..cw).map(move |x| (x, y)))
            .filter(|&(x, y)| px[((y * cw + x) * 4 + 3) as usize] > 0)
            .map(|(x, _)| x)
            .max()
            .expect("some glyph pixels");
        // Box centre x = 100, natural half-width = 100 (right edge 200), visible
        // half-width = 50 (right edge 150). Squished text must end before ~150.
        assert!(max_x <= 152, "squished text should not extend past the visible box (max_x = {max_x})");
    }

    #[test]
    fn scale_survives_edit_cycle() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let mut ed = empty_editor(&mut eng, ResizeMode::Fixed);
        ed.set_scale((0.6, 1.4));
        ed.insert_str(&mut eng, "abc");
        ed.backspace(&mut eng);
        let out = ed.to_content();
        assert_eq!(out.plain_text(), "ab");
        // Scale persists through editing; the natural layout box is unchanged
        // (the squish is a separate display property, not a layout change).
        assert_eq!(out.scale, (0.6, 1.4));
        assert!((out.box_rect.w - 200.0).abs() < 1e-3, "w {}", out.box_rect.w);
    }

    #[test]
    fn backspace_and_enter() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let mut ed = empty_editor(&mut eng, ResizeMode::Fixed);
        ed.insert_str(&mut eng, "ab");
        ed.enter(&mut eng);
        ed.insert_str(&mut eng, "cd");
        assert_eq!(ed.to_content().plain_text(), "ab\ncd");
        ed.backspace(&mut eng);
        assert_eq!(ed.to_content().plain_text(), "ab\nc");
    }

    #[test]
    fn select_all_then_copy() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let mut ed = empty_editor(&mut eng, ResizeMode::Fixed);
        ed.insert_str(&mut eng, "word");
        ed.select_all(&mut eng);
        assert!(ed.has_selection());
        assert_eq!(ed.copy().as_deref(), Some("word"));
        assert!(!ed.selection_rects(&mut eng).is_empty());
    }

    #[test]
    fn caret_present_after_typing() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let mut ed = empty_editor(&mut eng, ResizeMode::Fixed);
        ed.insert_str(&mut eng, "x");
        assert!(ed.caret_rect(&mut eng).is_some());
    }

    #[test]
    fn toggle_bold_applies_to_whole_text_when_unselected() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let mut ed = empty_editor(&mut eng, ResizeMode::Fixed);
        ed.insert_str(&mut eng, "hello");
        ed.toggle_bold(&mut eng);
        let c = ed.to_content();
        assert!(
            c.runs.iter().filter(|r| !r.text.is_empty()).all(|r| r.style.bold),
            "all text should be bold"
        );
        assert!(c.default_style.bold, "default style should follow whole-text toggle");
    }

    #[test]
    fn toggle_bold_applies_to_selection_only() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let mut ed = empty_editor(&mut eng, ResizeMode::Fixed);
        ed.insert_str(&mut eng, "hello");
        ed.move_home(&mut eng, false);
        ed.move_right(&mut eng, true);
        ed.move_right(&mut eng, true); // select "he"
        ed.toggle_bold(&mut eng);
        let c = ed.to_content();
        assert_eq!(c.plain_text(), "hello");
        let bold: String = c
            .runs
            .iter()
            .filter(|r| r.style.bold)
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(bold, "he", "only the selected range is bold");
        assert!(!c.default_style.bold, "default unchanged for a ranged toggle");
    }

    #[test]
    fn set_color_recolors_selection() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let mut ed = empty_editor(&mut eng, ResizeMode::Fixed);
        ed.insert_str(&mut eng, "abcd");
        ed.select_all(&mut eng);
        ed.set_color(&mut eng, Color::new(255, 0, 0));
        let c = ed.to_content();
        assert!(
            c.runs.iter().filter(|r| !r.text.is_empty()).all(|r| r.style.color == Color::new(255, 0, 0)),
            "all text should be red"
        );
    }

    #[test]
    fn auto_width_grows_box() {
        let mut eng = engine();
        if eng.available_families().is_empty() {
            return;
        }
        let mut ed = empty_editor(&mut eng, ResizeMode::AutoWidth);
        ed.insert_str(&mut eng, "i");
        let w_short = ed.box_rect().w;
        ed.insert_str(&mut eng, "iiiiiiiiiiiiiii");
        assert!(
            ed.box_rect().w > w_short,
            "auto-width box should grow with more text ({} > {w_short})",
            ed.box_rect().w
        );
    }
}
