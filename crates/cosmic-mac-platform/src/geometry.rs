//! Plain geometry types used by every platform primitive.
//!
//! All rectangles produced by this crate are in the **global top-left** coordinate
//! space used by AX, SkyLight and CoreGraphics: origin at the top-left corner of
//! the main display, `y` growing downwards. See [`crate::coords`] for conversion
//! to/from AppKit's bottom-left `NSScreen` space.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};

/// A point in global top-left coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    /// Horizontal offset from the main display's left edge.
    pub x: f64,
    /// Vertical offset from the main display's top edge (grows downwards).
    pub y: f64,
}

/// A size in points.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    /// Width in points.
    pub width: f64,
    /// Height in points.
    pub height: f64,
}

/// A rectangle in global top-left coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    /// Top-left corner.
    pub origin: Point,
    /// Extent.
    pub size: Size,
}

impl Point {
    /// Construct a point.
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

impl Size {
    /// Construct a size.
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

impl Rect {
    /// Construct a rectangle.
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            origin: Point::new(x, y),
            size: Size::new(width, height),
        }
    }

    /// Construct from a CoreGraphics rectangle.
    pub fn from_cg(rect: CGRect) -> Self {
        Self {
            origin: Point::new(rect.origin.x, rect.origin.y),
            size: Size::new(rect.size.width, rect.size.height),
        }
    }

    /// Convert to a CoreGraphics rectangle.
    pub fn to_cg(self) -> CGRect {
        CGRect::new(
            CGPoint::new(self.origin.x, self.origin.y),
            CGSize::new(self.size.width, self.size.height),
        )
    }

    /// `x + width`.
    pub fn max_x(self) -> f64 {
        self.origin.x + self.size.width
    }

    /// `y + height`.
    pub fn max_y(self) -> f64 {
        self.origin.y + self.size.height
    }

    /// Whether the rectangle has a positive area.
    pub fn is_non_empty(self) -> bool {
        self.size.width > 0.0 && self.size.height > 0.0
    }

    /// Shrink the rectangle by `amount` on every side.
    pub fn inset(self, amount: f64) -> Self {
        Self::new(
            self.origin.x + amount,
            self.origin.y + amount,
            (self.size.width - 2.0 * amount).max(0.0),
            (self.size.height - 2.0 * amount).max(0.0),
        )
    }

    /// Whether `point` lies inside the rectangle.
    pub fn contains(self, point: Point) -> bool {
        point.x >= self.origin.x
            && point.x < self.max_x()
            && point.y >= self.origin.y
            && point.y < self.max_y()
    }

    /// Whether two rectangles intersect with positive area.
    pub fn intersects(self, other: Self) -> bool {
        self.origin.x < other.max_x()
            && other.origin.x < self.max_x()
            && self.origin.y < other.max_y()
            && other.origin.y < self.max_y()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(r.max_x(), 110.0);
        assert_eq!(r.max_y(), 70.0);
        assert!(r.contains(Point::new(10.0, 20.0)));
        assert!(!r.contains(Point::new(110.0, 20.0)));
        assert!(r.intersects(Rect::new(100.0, 60.0, 50.0, 50.0)));
        assert!(!r.intersects(Rect::new(200.0, 200.0, 5.0, 5.0)));
    }
}
