//! Pure CPU pixel helpers shared across the workspace.
//!
//! Operates on flat BGRA8 row-major buffers with no padding, plus the
//! straight RGBA8 / RGB8 conversions encoders and decoders need.

mod affine;
mod convert;
mod crop;
mod sample;
mod scale;

pub use affine::transform_bgra8;
pub use convert::{
    premul_bgra8_over_white_to_rgb8, premul_bgra8_to_rgba8, rgb8_to_opaque_bgra8,
    straight_rgba8_to_premul_bgra8,
};
pub use crop::crop_bgra8;
pub use sample::{sample_bilinear, sample_nearest};
pub use scale::{scale, scale_bgra8_bilinear};
