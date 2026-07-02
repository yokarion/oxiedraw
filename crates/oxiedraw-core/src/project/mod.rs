//! `.oxiedrawproj` project file format and I/O.
//!
//! A project is a GNU TAR archive (forward-slash paths) with `manifest.json`
//! first, then `document.json`, then `layers/<layer-id>.png`:
//!
//! ```text
//! manifest.json     schema_version, app_version, created_at
//! document.json     canvas size/dpi, active layer, ordered layer list
//! layers/<id>.png   RGBA8 sRGB; R/B swapped against the BGRA Vulkan layers
//! ```
//!
//! Schema or canvas-size mismatches are rejected (`UnsupportedSchema`,
//! `CanvasSizeMismatch`). Unknown JSON fields and tar entries are ignored, so
//! older readers reject newer files but tolerate additive changes.

pub mod format;
pub mod load;
pub mod save;

pub use format::OxieProject;

use crate::renderer::RendererError;

/// Errors that can occur while saving or loading an `.oxiedrawproj` file.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("PNG error: {0}")]
    Png(String),
    #[error("archive is missing required entry: {0}")]
    MissingEntry(String),
    #[error("unsupported schema version {found} (this build supports version {expected})")]
    UnsupportedSchema { found: u32, expected: u32 },
    #[error("project canvas is {proj_w}x{proj_h} but the current canvas is {cur_w}x{cur_h}")]
    CanvasSizeMismatch {
        proj_w: u32,
        proj_h: u32,
        cur_w: u32,
        cur_h: u32,
    },
    #[error("renderer error: {0}")]
    Renderer(#[from] RendererError),
    #[error("project contains no layers")]
    NoLayers,
}

#[cfg(test)]
mod tests {
    use oxiedraw_utils::geometry::Size;

    use crate::canvas::Canvas;
    use crate::document::{DocumentProperties, LayerState};

    use super::{ProjectError, load, save};

    /// A text layer and the font it uses survive a save/load round-trip: the
    /// structured content reloads in the layer kind, and the font file bytes
    /// are embedded so it renders without the font installed.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn text_layer_and_fonts_round_trip() {
        use std::collections::HashSet;

        use crate::color::Color;
        use crate::document::LayerKind;
        use crate::text::fonts::TextEngine;
        use crate::text::{FontId, HAlign, ResizeMode, TextBox, TextContent, TextRun, TextStyle, VAlign};

        let size = Size::new(32, 32);
        let mut canvas = Canvas::headless(size).expect("canvas");
        let engine = TextEngine::new();
        let family = engine.default_family();

        let style = TextStyle::new(FontId::new(family.clone()), Color::BLACK);
        let content = TextContent {
            box_rect: TextBox::new(10.0, 10.0, 20.0, 12.0, 0.0),
            resize: ResizeMode::Fixed,
            h_align: HAlign::Left,
            v_align: VAlign::Top,
            runs: vec![TextRun::new("Hi", style.clone())],
            default_style: style,
            scale: (1.0, 1.0),
        };
        let px = vec![0u8; (32 * 32 * 4) as usize];
        let idx = canvas.add_layer_with_pixels("Text", &px).expect("add layer");
        canvas.layers().set_kind(idx, LayerKind::Text(content));

        let families: HashSet<String> = HashSet::from([family]);
        let registry = engine.embed_used_fonts(&families);

        let props = DocumentProperties { canvas: size, dpi: 96.0 };
        let path = std::env::temp_dir().join("oxiedraw_text_round_trip.oxiedrawproj");
        save::save(
            &mut canvas,
            &props,
            &crate::components::ComponentLibrary::new(),
            &registry,
            None,
            &path,
        )
        .expect("save");

        let project = load::load(&path).expect("load");
        let kind = &project
            .document
            .layers
            .iter()
            .find(|l| l.name == "Text")
            .expect("text layer present")
            .kind;
        assert!(
            matches!(kind, LayerKind::Text(c) if c.plain_text() == "Hi"),
            "text content must survive in the layer kind"
        );
        assert!(!project.fonts.is_empty(), "expected embedded font metadata");
        assert!(!project.font_bytes.is_empty(), "expected embedded font bytes");

        std::fs::remove_file(&path).ok();
    }

    /// Save a headless canvas and reload it into a fresh one. Verifies that
    /// manifest fields, layer metadata, and raw pixel bytes all survive the
    /// full tar/JSON/PNG round-trip without modification.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn round_trip() {
        let size = Size::new(64, 64);
        let mut canvas = Canvas::headless(size).expect("canvas init");

        // Paint the active layer so pixels are non-trivial.
        canvas.clear([0.8, 0.3, 0.1, 1.0]).expect("clear");
        let before = canvas.read_layer(0).expect("read before save");

        let props = DocumentProperties {
            canvas: size,
            dpi: 150.0,
        };
        let path = std::env::temp_dir().join("oxiedraw_round_trip.oxiedrawproj");

        save::save(&mut canvas, &props, &crate::components::ComponentLibrary::new(), &crate::text::fonts::FontRegistry::new(), None, &path)
            .expect("save");
        let project = load::load(&path).expect("load");

        assert_eq!(project.manifest.schema_version, super::format::SCHEMA_VERSION);
        assert_eq!(project.document.canvas_width, 64);
        assert_eq!(project.document.canvas_height, 64);
        assert_eq!(project.document.dpi, 150.0);
        assert_eq!(project.document.layers.len(), 1);
        assert_eq!(project.document.layers[0].name, "Background");
        assert!(project.document.layers[0].visible);
        assert_eq!(project.document.active_layer, Some(0));

        let mut canvas2 = Canvas::new(size, LayerState::new()).expect("canvas2 init");
        load::apply(&project, &mut canvas2).expect("apply");

        assert_eq!(canvas2.layers().len(), 1);
        assert_eq!(canvas2.layers().snapshot()[0].name, "Background");
        assert_eq!(canvas2.layers().active(), Some(0));

        let after = canvas2.read_layer(0).expect("read after load");
        assert_eq!(before, after, "pixel data must survive PNG round-trip");

        std::fs::remove_file(&path).ok();
    }

    /// The folder tree (schema v7) survives a save/load round-trip and is
    /// re-applied to the reloaded canvas, so folder-scoped adjustments persist.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn folder_tree_round_trip() {
        use crate::document::{LayerGroup, LayerTreeNode};

        let size = Size::new(32, 32);
        let mut canvas = Canvas::headless(size).expect("canvas");
        let px = vec![0u8; (32 * 32 * 4) as usize];
        let a = canvas.add_layer_with_pixels("A", &px).expect("add A");
        let b = canvas.add_layer_with_pixels("B", &px).expect("add B");
        let snap = canvas.layers().snapshot();
        let tree = vec![
            LayerTreeNode::layer(snap[a].id.clone()),
            LayerTreeNode::Group(LayerGroup {
                id: "g1".to_string(),
                name: "Folder".to_string(),
                expanded: true,
                children: vec![LayerTreeNode::layer(snap[b].id.clone())],
            }),
        ];
        canvas.set_layer_tree(tree.clone()).expect("set tree");

        let props = DocumentProperties { canvas: size, dpi: 96.0 };
        let path = std::env::temp_dir().join("oxiedraw_folder_round_trip.oxiedrawproj");
        save::save(
            &mut canvas,
            &props,
            &crate::components::ComponentLibrary::new(),
            &crate::text::fonts::FontRegistry::new(),
            None,
            &path,
        )
        .expect("save");

        let project = load::load(&path).expect("load");
        assert_eq!(project.document.layer_tree, tree, "tree must survive JSON");

        let mut canvas2 = Canvas::new(size, LayerState::new()).expect("canvas2");
        load::apply(&project, &mut canvas2).expect("apply");
        assert_eq!(canvas2.layer_tree(), tree.as_slice(), "tree must re-apply");

        std::fs::remove_file(&path).ok();
    }

    /// A pre-v7 `document.json` (no `layer_tree` field) deserializes with an
    /// empty tree, so older projects load as flat.
    #[test]
    fn pre_v7_document_loads_flat() {
        let json = r#"{
            "canvas_width": 16,
            "canvas_height": 16,
            "dpi": 96.0,
            "active_layer": 0,
            "layers": []
        }"#;
        let doc: super::format::DocumentData =
            serde_json::from_str(json).expect("pre-v7 document must still parse");
        assert!(doc.layer_tree.is_empty(), "missing tree must default to flat");
    }

    /// Loading a project saved at one canvas size into a canvas of a different
    /// size must fail with a clear `CanvasSizeMismatch` error.
    #[test]
    #[ignore = "requires vulkan loader and device"]
    fn canvas_size_mismatch() {
        let size_a = Size::new(64, 64);
        let size_b = Size::new(128, 128);
        let mut canvas_a = Canvas::headless(size_a).expect("canvas a");

        let props = DocumentProperties {
            canvas: size_a,
            dpi: 96.0,
        };
        let path = std::env::temp_dir().join("oxiedraw_size_mismatch.oxiedrawproj");

        save::save(&mut canvas_a, &props, &crate::components::ComponentLibrary::new(), &crate::text::fonts::FontRegistry::new(), None, &path)
            .expect("save");
        let project = load::load(&path).expect("load");

        let mut canvas_b = Canvas::new(size_b, LayerState::new()).expect("canvas b");
        let err = load::apply(&project, &mut canvas_b).expect_err("should fail");

        assert!(
            matches!(
                err,
                ProjectError::CanvasSizeMismatch {
                    proj_w: 64,
                    proj_h: 64,
                    cur_w: 128,
                    cur_h: 128
                }
            ),
            "unexpected error: {err}"
        );

        std::fs::remove_file(&path).ok();
    }
}
