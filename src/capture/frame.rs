// Geometry shared by capture, selection and output.
#![allow(dead_code)]

use super::pixels::DibPixels;

/// A point in virtual-desktop physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A half-open rectangle in virtual-desktop physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub const fn width(&self) -> i32 {
        self.right - self.left
    }

    pub const fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub const fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    pub fn normalize(a: Point, b: Point) -> Self {
        Self {
            left: a.x.min(b.x),
            top: a.y.min(b.y),
            right: a.x.max(b.x),
            bottom: a.y.max(b.y),
        }
    }

    pub fn intersect(&self, other: Rect) -> Option<Rect> {
        let result = Rect {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        (!result.is_empty()).then_some(result)
    }

    pub fn union(&self, other: Rect) -> Rect {
        Rect {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    pub fn clamp(&self, bounds: Rect) -> Rect {
        if bounds.is_empty() {
            return bounds;
        }
        let left = self.left.clamp(bounds.left, bounds.right);
        let top = self.top.clamp(bounds.top, bounds.bottom);
        let right = self.right.clamp(bounds.left, bounds.right);
        let bottom = self.bottom.clamp(bounds.top, bounds.bottom);
        Rect {
            left: left.min(right),
            top: top.min(bottom),
            right,
            bottom,
        }
    }

    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect {
            left: self.left + dx,
            top: self.top + dy,
            right: self.right + dx,
            bottom: self.bottom + dy,
        }
    }

    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.left && point.x < self.right && point.y >= self.top && point.y < self.bottom
    }
}

/// A top-down 32-bit BGRA frame. Never derive `Clone`: copies are explicit.
#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pixels: FramePixels,
}

#[derive(Debug)]
enum FramePixels {
    Owned(Vec<u8>),
    Dib(DibPixels),
}

impl FramePixels {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Owned(pixels) => pixels,
            Self::Dib(pixels) => pixels.as_slice(),
        }
    }
}

impl PartialEq for Frame {
    fn eq(&self, other: &Self) -> bool {
        (self.width, self.height, self.stride) == (other.width, other.height, other.stride)
            && self.pixels() == other.pixels()
    }
}

impl Eq for Frame {}

impl Frame {
    pub fn new(width: u32, height: u32, bgra: Vec<u8>) -> Option<Self> {
        Self::with_pixels(width, height, FramePixels::Owned(bgra))
    }

    pub(super) fn from_dib(width: u32, height: u32, pixels: DibPixels) -> Option<Self> {
        Self::with_pixels(width, height, FramePixels::Dib(pixels))
    }

    fn with_pixels(width: u32, height: u32, pixels: FramePixels) -> Option<Self> {
        let stride = width.checked_mul(4)?;
        let expected = (stride as usize).checked_mul(height as usize)?;
        (pixels.as_slice().len() == expected).then_some(Self {
            width,
            height,
            stride,
            pixels,
        })
    }

    pub fn empty() -> Self {
        Self {
            width: 0,
            height: 0,
            stride: 0,
            pixels: FramePixels::Owned(Vec::new()),
        }
    }

    pub fn pixels(&self) -> &[u8] {
        self.pixels.as_slice()
    }

    pub fn row(&self, y: u32) -> &[u8] {
        let start = y as usize * self.stride as usize;
        &self.pixels()[start..start + self.stride as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handles_reverse_drag() {
        let r = Rect::normalize(Point::new(100, 80), Point::new(10, 20));
        assert_eq!(r, Rect::new(10, 20, 100, 80));
        assert_eq!(r.width(), 90);
        assert_eq!(r.height(), 60);
    }

    #[test]
    fn intersect_returns_none_for_disjoint() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(200, 200, 300, 300);
        assert_eq!(a.intersect(b), None);
    }

    #[test]
    fn intersect_handles_cross_monitor() {
        let a = Rect::new(-1920, 0, 0, 1080);
        let b = Rect::new(-100, 100, 500, 500);
        assert_eq!(a.intersect(b), Some(Rect::new(-100, 100, 0, 500)));
    }

    #[test]
    fn union_covers_both() {
        let a = Rect::new(-10, -10, 10, 10);
        let b = Rect::new(5, 5, 20, 20);
        assert_eq!(a.union(b), Rect::new(-10, -10, 20, 20));
    }

    #[test]
    fn clamp_keeps_rect_inside_bounds() {
        let bounds = Rect::new(0, 0, 100, 100);
        assert_eq!(
            Rect::new(-20, -20, 50, 50).clamp(bounds),
            Rect::new(0, 0, 50, 50)
        );
        assert_eq!(
            Rect::new(80, 80, 200, 200).clamp(bounds),
            Rect::new(80, 80, 100, 100)
        );
    }

    #[test]
    fn frame_rejects_wrong_buffer_size() {
        assert!(Frame::new(2, 2, vec![0; 16]).is_some());
        assert!(Frame::new(2, 2, vec![0; 15]).is_none());
    }
}
