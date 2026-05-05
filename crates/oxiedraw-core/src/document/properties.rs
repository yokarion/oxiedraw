use oxiedraw_utils::geometry::Size;

#[derive(Debug, Clone)]
pub struct DocumentProperties {
    pub canvas: Size,
    pub dpi: f32,
}
