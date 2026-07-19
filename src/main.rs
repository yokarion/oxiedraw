//! OxieDraw binary entry point.
//!
//! # Crates
//!
//! `oxiedraw-utils` (no workspace deps) -> `oxiedraw-core` (engine, GPU, state)
//! -> `oxiedraw-ui` (relm4/libadwaita, the only crate that touches UI
//! libraries). Types that cross the UI/core boundary live in `core`; core never
//! reaches back into the UI.
//!
//! # Where things live
//!
//! - `oxiedraw_core::canvas::Canvas` - the only handle to the GPU. Owns the
//!   Vulkan renderer; the UI never calls `ash` directly.
//! - `oxiedraw_core::renderer` - GPU pipelines, layer compositing and the dmabuf
//!   display path. Its module docs carry the image/pipeline tables.
//! - `oxiedraw_ui::session` - one `DocumentSession` per open tab, plus the
//!   `GlobalState` shared across tabs (brushes, colours, fonts, clipboard).
//! - `oxiedraw_core::history` - every undoable edit is one `HistoryAction`
//!   variant. The exhaustive match is deliberate: adding a variant makes the
//!   compiler point at everything that must handle it.
//!
//! # Platform
//!
//! Linux/Wayland only, by design. The canvas is presented zero-copy: a Vulkan
//! image is exported as a dmabuf that GTK imports directly, so the GPU writes
//! the displayed pixels once. That needs `VK_EXT_image_drm_format_modifier` and
//! therefore a real GPU - it does not run on lavapipe, which is why the GPU
//! tests are `#[ignore]`d and skipped in CI (run them locally with
//! `cargo test -- --ignored`). Porting to another OS means writing a second
//! present backend plus its display integration, not reworking the engine.
//!
//! # Latency-critical paths
//!
//! A few pieces look like over-complication and are load-bearing fixes for bugs
//! that were expensive to find. Read their module docs before changing them:
//!
//! - `oxiedraw_ui::canvas::RenderPump` - keeps the GTK frame clock alive during
//!   a stylus drag, so coalesced motion bursts don't cost a re-sync frame.
//! - `renderer::vulkan::present` - the clipped per-frame present, and
//!   `DISPLAY_BUFFERS == 1` which that clipping depends on for correctness.
//! - `renderer::vulkan::stroke` - per-ring-slot dab buffer regions, which stop
//!   fast strokes dropping their tail dabs.
//! - `brush_engine::stamp` - the stabiliser EMAs are rescaled to a fixed
//!   reference interval so smoothing doesn't vary with input rate.

use std::process::ExitCode;

fn main() -> ExitCode {
    oxiedraw_utils::tracing::init();
    oxiedraw_ui::run()
}
