//! Coordinate-space conversion between AppKit (bottom-left) and AX/SLS
//! (top-left, global) coordinates.
//!
//! macOS has two global coordinate spaces:
//!
//! * **AX / SkyLight / CGWindow / CGDisplay**: origin at the top-left of the
//!   *main* display, `y` grows downwards. This is what [`crate::Rect`] uses.
//! * **NSScreen / NSWindow**: origin at the bottom-left of the *main* display,
//!   `y` grows upwards. `NSScreen::frame()` and `visibleFrame()` are in this
//!   space.
//!
//! The flip is anchored on the main display's height, not on the height of the
//! display the rectangle belongs to. Displays placed above the main display
//! therefore have negative `y` in the top-left space, exactly like CoreGraphics
//! reports them.

use crate::{Point, Rect};

/// Flip a vertical extent from one space to the other.
fn flip_y(y: f64, height: f64, main_height: f64) -> f64 {
    main_height - (y + height)
}

/// Convert an AppKit (bottom-left) rectangle to the AX/SLS (top-left) space.
pub fn ns_to_ax(rect: Rect, main_display_height: f64) -> Rect {
    Rect::new(
        rect.origin.x,
        flip_y(rect.origin.y, rect.size.height, main_display_height),
        rect.size.width,
        rect.size.height,
    )
}

/// Convert an AX/SLS (top-left) rectangle to the AppKit (bottom-left) space.
pub fn ax_to_ns(rect: Rect, main_display_height: f64) -> Rect {
    Rect::new(
        rect.origin.x,
        flip_y(rect.origin.y, rect.size.height, main_display_height),
        rect.size.width,
        rect.size.height,
    )
}

/// Convert an AppKit point to the AX/SLS space.
pub fn ns_point_to_ax(point: Point, main_display_height: f64) -> Point {
    Point::new(point.x, main_display_height - point.y)
}

/// Convert an AX/SLS point to the AppKit space.
pub fn ax_point_to_ns(point: Point, main_display_height: f64) -> Point {
    Point::new(point.x, main_display_height - point.y)
}

/// The AX/SLS-space frame of the display that contains `point`.
///
/// Falls back to the first display when the point is outside every display
/// (which happens with stale coordinates after a display reconfiguration).
pub fn display_containing(
    displays: &[crate::display::DisplayInfo],
    point: Point,
) -> Option<&crate::display::DisplayInfo> {
    displays.iter().find(|d| d.frame.contains(point))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let main = 1080.0;
        let ax = Rect::new(10.0, 20.0, 100.0, 50.0);
        let ns = ax_to_ns(ax, main);
        assert_eq!(ns.origin.y, 1080.0 - 70.0);
        assert_eq!(ns_to_ax(ns, main), ax);
    }

    #[test]
    fn secondary_display_above_main_is_negative() {
        let main = 1080.0;
        // A display whose AppKit origin is (0, 1080), 1920x1080.
        let ns = Rect::new(0.0, 1080.0, 1920.0, 1080.0);
        let ax = ns_to_ax(ns, main);
        assert_eq!(ax.origin.y, -1080.0);
    }
}
