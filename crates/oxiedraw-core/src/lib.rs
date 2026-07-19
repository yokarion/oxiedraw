//! OxieDraw engine and state - everything below the UI.
//!
//! [`canvas::Canvas`] is the front door: it owns the Vulkan renderer and is the
//! only type the UI drives to touch the GPU. A [`document::Document`] holds the
//! layer tree and properties; [`history::HistoryStack`] records every mutation
//! as one [`history::HistoryAction`] variant, so adding an undoable operation
//! means adding a variant and letting the exhaustive match force the rest.
//!
//! Per-document state (layers, history, tool state) is owned by
//! `oxiedraw_ui::session::DocumentSession`, one per open tab; state shared
//! across tabs (brushes, colours, fonts) lives in its `GlobalState`. Nothing in
//! this crate reaches back into the UI.
//!
//! Brush presets are plain data (`brush_engine::preset`), not trait impls; the
//! shared stamping code lives in `brush_engine::stamp`.
//!
//! Anything a high-frequency input closure mutates lives behind `Rc<Cell<_>>`
//! or `Rc<RefCell<_>>`, so a pointer-motion handler can update it without
//! routing through the relm4 message loop. The renderer is single-threaded and
//! has no async; see [`renderer`] for the GPU pipeline and the Linux dmabuf
//! display path.

pub mod brush_engine;
pub mod canvas;
pub mod color;
pub mod components;
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
