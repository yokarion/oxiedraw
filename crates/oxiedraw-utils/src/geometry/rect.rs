use super::Point;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Size {
    pub const ZERO: Self = Self {
        width: 0,
        height: 0,
    };

    #[inline]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    #[inline]
    pub const fn area(self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

impl Rect {
    #[inline]
    pub const fn new(origin: Point, size: Size) -> Self {
        Self { origin, size }
    }
}
