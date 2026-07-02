//! Headless Vulkan renderer.
//!
//! The renderer owns every GPU resource. `Canvas` (in `crate::canvas`) is
//! the public API; brush handlers and the UI never touch Vulkan directly.
//!
//! # Pipeline
//!
//! ```text
//! pointer event -> brush engine -> dab list -> mask pipeline stamps coverage
//!                                                  v
//!                                      stroke buffer (R8)
//!                                                  v   commit
//!                                      active layer image (BGRA)
//!                                                  v   composite all layers
//!                                      canvas image (BGRA)
//!                                                  v   copy
//!                                      dmabuf image <- GTK reads directly
//!                                                  v
//!                                              screen
//! ```
//!
//! Three big ideas:
//!
//! 1. The canvas image is a composite output, not the source of truth.
//!    Each layer has its own GPU image; canvas is rebuilt from the stack
//!    whenever layers change.
//! 2. In-flight strokes live in a single-channel R8 scratch image
//!    (`stroke`) until the user releases the pointer. On release the
//!    coverage is tinted with the user's color and composited onto the
//!    target layer.
//! 3. Display is zero-copy via Linux dmabuf. The GPU writes the displayed
//!    image once; GTK reads the same memory.
//!
//! # GPU images
//!
//! | Image              | Format            | Role                                          |
//! | ------------------ | ----------------- | --------------------------------------------- |
//! | `stroke`           | `R8_UNORM`        | Per-pixel coverage of the in-flight stroke.   |
//! | Per-layer images   | `B8G8R8A8_SRGB`   | The actual painted content, one per layer.    |
//! | `canvas`           | `B8G8R8A8_SRGB`   | Composited stack when not stroking.           |
//! | `preview`          | `B8G8R8A8_SRGB`   | Stack + in-flight stroke at correct z-order.  |
//! | `display` (dmabuf) | `B8G8R8A8_SRGB`   | LINEAR-tiled, the image GTK reads.            |
//!
//! BGRA byte order matches `cairo::Format::ARgb32` and DRM fourcc
//! `ARGB8888`. Linear-space color, premultiplied alpha; sRGB encoding is
//! handled by the hardware on writes.
//!
//! # Pipelines
//!
//! | Pipeline          | Reads             | Writes to        | Purpose                                       |
//! | ----------------- | ----------------- | ---------------- | --------------------------------------------- |
//! | `dab`             | (none)            | BGRA framebuffer | Rasterizes one circle per instance, soft AA.  |
//! | `mask`            | (none)            | stroke (R8, MAX) | Coverage-only, builds up stroke mask.         |
//! | `composite`       | stroke sampler    | BGRA framebuffer | tint * coverage, OVER blend.                  |
//! | `layer_composite` | one layer sampler | BGRA framebuffer | layer pixel, OVER blend.                      |
//! | `transform`       | source sampler    | BGRA framebuffer | Affine remap of one source into a target.     |
//!
//! Plus a `vkCmdCopyImage` for the final "canvas/preview -> dmabuf" step.
//!
//! # Stroke flow
//!
//! ```text
//! drag_begin -> Canvas::begin_stroke -> clear stroke buffer
//! drag_update -> Canvas::stamp        -> mask pipeline into stroke buffer
//!                Canvas::present      -> preview = composite layers + tinted stroke
//!                                       -> copy preview -> dmabuf
//! drag_end   -> Canvas::commit_stroke -> composite stroke into active layer
//!                                       -> clear stroke buffer
//!                                       -> recomposite canvas from layers
//! ```
//!
//! The stroke buffer holds coverage with MAX blending: overlapping dabs
//! don't darken because the max coverage wins per pixel. On commit we
//! tint the saturated coverage once with the user's color and OVER-blend
//! onto the target layer.
//!
//! # Layers and z-order
//!
//! `LayerState.layers[i]` is the metadata for the layer whose pixels
//! live in `LayerStack.slots[i].image`. Index 0 is bottom, last index is
//! top. Every mutation goes through `Canvas` so both stay aligned.
//!
//! The preview path reproduces the same composite order as the
//! post-commit canvas, with the stroke spliced in at the target layer's
//! z position, so mid-stroke and post-commit look identical at release.
//! Stroke into a hidden target is omitted from the preview (matches
//! commit semantics).
//!
//! # dmabuf path
//!
//! GTK4 has no `VulkanArea`. The naive `GPU image -> staging buffer ->
//! Vec<u8> -> cairo` round-trip is ~2.4 ms/frame at 2048^2 - slower than
//! cairo. The dmabuf path is `GPU image -> vkCmdCopyImage -> dmabuf
//! image` (~0.36 ms/frame), with GTK importing the fd via
//! `gdk::DmabufTextureBuilder`.
//!
//! We pick `DRM_FORMAT_MOD_LINEAR` (universally supported). The fast
//! AMD path (AFBC) is rejected by GTK/Mutter. The fd is wrapped in
//! `Arc<OwnedFd>`; we `dup(2)` per texture build so GTK can close its
//! copy without affecting ours.
//!
//! # Shaders
//!
//! GLSL sources in `crates/oxiedraw-core/shaders/`, compiled to SPIR-V
//! at build time by `build.rs` shelling out to `glslc` (target
//! `vulkan1.3`, `-O`). No `shaderc-rs` dependency.
//!
//! # Gotchas
//!
//! - `vkCmdClearColorImage` on sRGB images takes linear-space floats.
//! - `gpu-allocator` requires `&mut Allocator` to free, so resource
//!   wrappers expose `unsafe fn destroy(self, device, allocator)`.
//!   No `Drop` impls on resource wrappers.
//! - Every pointer motion event triggers one stamp submit + one present
//!   submit. Fusing them is a known optimization (TODO).

mod composite;
mod dab;
mod device;
mod dmabuf;
mod erase;
mod error;
mod fill_overlay;
mod filters;
mod gradient_overlay;
mod instance;
mod layers;
mod mask;
mod pattern_atlas;
mod resources;
mod selection;
mod shape_overlay;
mod targets;
mod transform;
mod vulkan;

pub use dmabuf::DmabufDescriptor;

pub use dab::{DabFamily, DabInstance};
pub use error::RendererError;
pub use layers::MAX_LAYERS;
pub use selection::SelectionBlendMode;
pub use vulkan::{
    CANVAS_FORMAT, EdgesBuffer, GradientKind, PresentSource, STROKE_FORMAT, ShapeKind,
    VulkanRenderer,
};
