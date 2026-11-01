// SPDX-License-Identifier: GPL-3.0-or-later
//! Platform-neutral types shared by `cosmic-mac-layout`, the `cosmic-mac-wm` daemon and
//! `cosmic-mac-ctl`.
//!
//! Nothing in this crate depends on macOS, Smithay or Wayland.  The types deliberately mirror
//! the *semantics* of the upstream COSMIC protocols where that is meaningful:
//!
//! * [`toplevel`] mirrors `cosmic_toplevel_info_v1` / `cosmic_toplevel_management_v1`.
//! * [`workspace`] mirrors `cosmic_workspace_v2`, simplified to a single ordered pool of
//!   workspaces (the daemon owns the display assignment).
//! * [`Action`] mirrors the window-management subset of COSMIC's keybinding actions.
//! * [`Command`] is the output of the pure layout engine; the daemon translates each command
//!   into Accessibility / SkyLight calls.
//!
//! # Coordinates
//!
//! All geometry is in global, top-left-origin logical pixels (the coordinate space used by the
//! macOS Accessibility API).  [`Rect`] stores `f64` because AX returns fractional frames, but
//! layout arithmetic keeps frames pixel-aligned in practice.
//!
//! # Orientation naming
//!
//! [`Orientation::Horizontal`] means "children are laid out left-to-right" (a vertical split
//! line).  Upstream `cosmic-comp` calls this `Vertical` because it names the split axis; the
//! more intuitive child-layout naming is used here.  This is the only intentional naming
//! difference from upstream.

mod action;
mod command;
mod config;
mod ipc;
mod shell;
mod toplevel;
mod tree;
mod workspace;

pub mod launcher;
pub mod theme;

pub use action::{Action, Axis, Direction};
pub use command::Command;
pub use config::{Config, ConfigChanged, Gaps, TilingException};
pub use ipc::{Request, Response};
pub use shell::ShellState;
pub use toplevel::{
    TabInfo, ToplevelCapability, ToplevelInfo, ToplevelManagement, ToplevelState, WindowState,
};
pub use tree::TreeNode;
pub use workspace::{Orientation, STACK_BAR_HEIGHT, StackInfo, WorkspaceGroup, WorkspaceInfo};

use serde::{Deserialize, Serialize};

/// Opaque window identifier.
///
/// The daemon uses the SkyLight / CoreGraphics window number, which is stable for the lifetime
/// of a window and fits comfortably in `u64`.
pub type WindowId = u64;

/// A width/height pair in logical pixels.
///
/// Used for per-window size constraints carried in [`ToplevelInfo`].  A maximum of
/// [`Size::UNBOUNDED`] (or any value at or above `f64::MAX / 2.0`) means "no maximum on this
/// axis"; JSON can represent it, unlike an infinite float.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Size {
    /// Width in logical pixels.
    pub width: f64,
    /// Height in logical pixels.
    pub height: f64,
}

impl Size {
    /// A zero size.
    pub const ZERO: Size = Size {
        width: 0.0,
        height: 0.0,
    };

    /// Sentinel maximum meaning "unbounded on both axes".
    pub const UNBOUNDED: Size = Size {
        width: f64::MAX,
        height: f64::MAX,
    };

    /// Construct a size.
    pub const fn new(width: f64, height: f64) -> Self {
        Size { width, height }
    }

    /// True when both axes are at the [`Size::UNBOUNDED`] sentinel.
    pub fn is_unbounded(self) -> bool {
        self.width >= f64::MAX / 2.0 && self.height >= f64::MAX / 2.0
    }
}

/// A rectangle in top-left-origin logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const ZERO: Rect = Rect {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };

    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// Right edge (exclusive).
    pub fn right(self) -> f64 {
        self.x + self.width
    }

    /// Bottom edge (exclusive).
    pub fn bottom(self) -> f64 {
        self.y + self.height
    }

    pub fn center(self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn area(self) -> f64 {
        self.width.max(0.0) * self.height.max(0.0)
    }

    pub fn is_empty(self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// Shrink by `gap` on every side, clamping at zero size.
    pub fn inset(self, gap: f64) -> Self {
        self.inset_sides(gap, gap, gap, gap)
    }

    /// Shrink by individual side amounts, clamping at zero size.
    pub fn inset_sides(self, left: f64, top: f64, right: f64, bottom: f64) -> Self {
        let x = self.x + left;
        let y = self.y + top;
        let width = (self.width - left - right).max(0.0);
        let height = (self.height - top - bottom).max(0.0);
        Rect::new(x, y, width, height)
    }

    pub fn contains_point(self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    pub fn intersection(self, other: Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right > x && bottom > y {
            Some(Rect::new(x, y, right - x, bottom - y))
        } else {
            None
        }
    }

    pub fn intersection_area(self, other: Rect) -> f64 {
        self.intersection(other).map(Rect::area).unwrap_or(0.0)
    }

    /// True when the two rectangles share a positive-area region.  Edge-adjacent rectangles do
    /// not overlap.
    pub fn overlaps(self, other: Rect) -> bool {
        self.intersection_area(other) > 0.0
    }

    pub fn translate(self, dx: f64, dy: f64) -> Self {
        Rect::new(self.x + dx, self.y + dy, self.width, self.height)
    }

    /// Fit the rectangle inside `bounds`: the size is clamped to the bounds and the position is
    /// moved so that the whole rectangle is inside.  A rectangle larger than the bounds is
    /// anchored at the top-left.
    pub fn clamp_within(self, bounds: Rect) -> Self {
        let width = self.width.min(bounds.width).max(0.0);
        let height = self.height.min(bounds.height).max(0.0);
        let max_x = (bounds.right() - width).max(bounds.x);
        let max_y = (bounds.bottom() - height).max(bounds.y);
        Rect::new(
            self.x.clamp(bounds.x, max_x),
            self.y.clamp(bounds.y, max_y),
            width,
            height,
        )
    }

    /// Compare all components with an absolute tolerance.  Only useful in tests and assertions.
    pub fn approx_eq(self, other: Rect, epsilon: f64) -> bool {
        (self.x - other.x).abs() <= epsilon
            && (self.y - other.y).abs() <= epsilon
            && (self.width - other.width).abs() <= epsilon
            && (self.height - other.height).abs() <= epsilon
    }
}

#[cfg(test)]
mod rect_tests {
    use super::Rect;

    #[test]
    fn edges_and_center() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(r.right(), 110.0);
        assert_eq!(r.bottom(), 70.0);
        assert_eq!(r.center(), (60.0, 45.0));
        assert_eq!(r.area(), 5000.0);
    }

    #[test]
    fn inset_clamps_to_zero() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(r.inset(5.0), Rect::new(5.0, 5.0, 0.0, 0.0));
        assert_eq!(r.inset(20.0), Rect::new(20.0, 20.0, 0.0, 0.0));
    }

    #[test]
    fn intersection_and_overlap() {
        let a = Rect::new(0.0, 0.0, 100.0, 100.0);
        let b = Rect::new(50.0, 50.0, 100.0, 100.0);
        assert_eq!(a.intersection(b), Some(Rect::new(50.0, 50.0, 50.0, 50.0)));
        assert_eq!(a.intersection_area(b), 2500.0);
        assert!(a.overlaps(b));
        let edge = Rect::new(100.0, 0.0, 100.0, 100.0);
        assert_eq!(a.intersection_area(edge), 0.0);
        assert!(!a.overlaps(edge));
    }

    #[test]
    fn clamp_within_moves_and_shrinks() {
        let bounds = Rect::new(0.0, 0.0, 800.0, 600.0);
        let r = Rect::new(700.0, 500.0, 200.0, 200.0);
        assert_eq!(
            r.clamp_within(bounds),
            Rect::new(600.0, 400.0, 200.0, 200.0)
        );
        let huge = Rect::new(100.0, 100.0, 1000.0, 1000.0);
        assert_eq!(huge.clamp_within(bounds), Rect::new(0.0, 0.0, 800.0, 600.0));
    }
}
