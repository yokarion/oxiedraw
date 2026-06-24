//! OxieDraw engine and state.
//!
//! State concentrates in `EngineState`, owned by the root relm4 component in
//! `oxiedraw-ui`, with each sub-system (`Document`/`LayerState`, `BrushEngine`,
//! `Compositor`, `ColorState`, `ToolState`) owning its own typed state. Brush
//! presets are plain data (`brush_engine::preset`), not trait impls; the shared
//! stamping code lives in `brush_engine::stamp`.
//!
//! Anything a high-frequency input closure mutates lives behind `Rc<Cell<_>>`
//! or `Rc<RefCell<_>>` so it can be updated without routing through the relm4
//! message loop; mutation outside hot paths goes through `&mut` in `update()`.

pub mod brush_engine;
pub mod canvas;
pub mod color;
pub mod components;
pub mod compositor;
pub mod document;
pub mod effects;
pub mod export;
pub mod filters;
pub mod history;
pub mod project;
pub mod renderer;
pub mod selection;
pub mod shape_correction;
pub mod shapes;
pub mod text;
pub mod tools;

use std::cell::RefCell;
use std::rc::Rc;

use brush_engine::BrushEngine;
use color::ColorState;
use compositor::Compositor;
use document::Document;
use history::{HistoryConfig, HistoryStack};
use text::fonts::FontRegistry;
use tools::{CropState, FillState, SelectionState, ShapeState, ToolState, TransformState};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("renderer: {0}")]
    Renderer(#[from] renderer::RendererError),
}

#[derive(Debug)]
pub struct EngineState {
    pub document: Document,
    pub brush_engine: BrushEngine,
    pub compositor: Compositor,
    pub colors: ColorState,
    pub tools: ToolState,
    pub crop: CropState,
    pub transform: TransformState,
    pub selection_state: SelectionState,
    pub fill: FillState,
    pub shape: ShapeState,
    /// Fonts embedded in this document so text renders without the font being
    /// installed. The shared `TextEngine` (system font database) is owned by
    /// the app, not here, since it is not per-document.
    pub fonts: FontRegistry,
    /// Undo/redo stack. Shared via `Rc<RefCell<_>>` so input-driven UI
    /// closures can `record()` without taking `&mut EngineState`.
    pub history: Rc<RefCell<HistoryStack>>,
}

impl EngineState {
    pub fn new(canvas: oxiedraw_utils::geometry::Size) -> Self {
        Self::with_history_config(canvas, HistoryConfig::default())
    }

    pub fn with_history_config(
        canvas: oxiedraw_utils::geometry::Size,
        history_config: HistoryConfig,
    ) -> Self {
        Self {
            document: Document::new(canvas),
            brush_engine: BrushEngine::new(),
            compositor: Compositor::new(),
            colors: ColorState::new(),
            tools: ToolState::new(),
            crop: CropState::new(),
            transform: TransformState::new(),
            selection_state: SelectionState::new(),
            fill: FillState::new(),
            shape: ShapeState::new(),
            fonts: FontRegistry::new(),
            history: Rc::new(RefCell::new(HistoryStack::new(history_config))),
        }
    }
}
