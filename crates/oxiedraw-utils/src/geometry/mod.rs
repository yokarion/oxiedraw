//! 2-D geometry primitives and polyline helpers shared across the workspace.

mod point;
mod polyline;
mod rect;
mod transform;

pub use point::Point;
pub use polyline::{arc_length, bounding_box, morph_path, resample};
pub use rect::{Rect, Size};
pub use transform::{TransformFilter, TransformRect};
