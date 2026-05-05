use oxiedraw_utils::geometry::Point;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputSample {
    pub position: Point,
    pub pressure: f32,
    pub tilt_x: f32,
    pub tilt_y: f32,
    /// Tablet pen barrel rotation (twist axis) in radians. `0.0` when
    /// the device doesn't report rotation.
    pub rotation: f32,
    pub time_ms: u64,
}
