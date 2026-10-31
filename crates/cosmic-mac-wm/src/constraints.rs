// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-window size constraints learned from the platform.
//!
//! macOS apps silently clamp Accessibility resizes.  When the daemon notices
//! that a window did not reach its tile, it probes the window's minimum size and
//! feeds the result back into the layout through
//! [`cosmic_mac_layout::Layout::update_window`], so the tiling tree can clamp
//! its tiles (and rebalance the siblings) instead of leaving a gap where the
//! wallpaper shows through.
//!
//! The learned values are cached for the rest of the session: probing moves the
//! window for a moment, so the daemon must never re-probe the same window on
//! every layout change.

use cosmic_mac_protocol::{Size, ToplevelInfo};

/// What the daemon has learned about one window's reachable geometry.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LearnedConstraints {
    /// Smallest size the window accepts.
    pub min: Option<Size>,
    /// Largest size the window accepts.  Axes without a learned maximum stay
    /// [`Size::UNBOUNDED`] so the layout treats them as free.
    pub max: Option<Size>,
    /// Whether the size probe already ran (successfully or not).
    pub probed: bool,
    /// Whether the "cannot reach its tile" warning was logged for the width.
    pub warned_width: bool,
    /// Whether the "cannot reach its tile" warning was logged for the height.
    pub warned_height: bool,
}

impl LearnedConstraints {
    /// Pin the width: the app refuses to change it, so the layout must ask for
    /// exactly this value.
    pub fn freeze_width(&mut self, value: f64) {
        let min = self.min.unwrap_or(Size::ZERO);
        self.min = Some(Size::new(min.width.max(value), min.height));
        let max = self.max.unwrap_or(Size::UNBOUNDED);
        self.max = Some(Size::new(value, max.height));
    }

    /// Pin the height, see [`LearnedConstraints::freeze_width`].
    pub fn freeze_height(&mut self, value: f64) {
        let min = self.min.unwrap_or(Size::ZERO);
        self.min = Some(Size::new(min.width, min.height.max(value)));
        let max = self.max.unwrap_or(Size::UNBOUNDED);
        self.max = Some(Size::new(max.width, value));
    }
}

/// Counters for diagnostics and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConstraintStats {
    /// Size probes issued.
    pub probes: usize,
    /// Layout re-tiles triggered by a clamped frame.
    pub retiles: usize,
    /// "window cannot reach its tile" warnings logged.
    pub warnings: usize,
}

/// Attach the learned constraints to a [`ToplevelInfo`] before handing it to
/// [`cosmic_mac_layout::Layout::update_window`].
///
/// The layout clamps each tile to `[min_size, max_size]` and redistributes the
/// difference among the siblings that can still grow or shrink, which is what
/// makes the tiled frames flush again.
pub fn attach(mut info: ToplevelInfo, constraints: LearnedConstraints) -> ToplevelInfo {
    if let Some(min) = constraints.min {
        info.min_size = Some(min);
    }
    if let Some(max) = constraints.max {
        info.max_size = Some(max);
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic_mac_protocol::Rect;

    fn info() -> ToplevelInfo {
        ToplevelInfo::new(7, "com.example.app", "Document")
            .with_initial_frame(Rect::new(0.0, 0.0, 300.0, 200.0))
    }

    #[test]
    fn defaults_have_no_constraints() {
        let constraints = LearnedConstraints::default();
        assert!(constraints.min.is_none());
        assert!(constraints.max.is_none());
        assert!(!constraints.probed);
    }

    #[test]
    fn attach_carries_min_and_max_into_the_layout_input() {
        let constraints = LearnedConstraints {
            min: Some(Size::new(400.0, 300.0)),
            max: Some(Size::new(600.0, 500.0)),
            probed: true,
            warned_width: false,
            warned_height: false,
        };
        let attached = attach(info(), constraints);
        assert_eq!(attached.min_size, Some(Size::new(400.0, 300.0)));
        assert_eq!(attached.max_size, Some(Size::new(600.0, 500.0)));
        assert_eq!(attached.wid, 7);
    }

    #[test]
    fn a_frozen_width_pins_both_bounds_for_that_axis() {
        let mut constraints = LearnedConstraints {
            min: Some(Size::new(400.0, 200.0)),
            ..LearnedConstraints::default()
        };
        constraints.freeze_width(715.0);
        assert_eq!(constraints.min, Some(Size::new(715.0, 200.0)));
        let max = constraints.max.expect("max");
        assert_eq!(max.width, 715.0);
        assert_eq!(max.height, f64::MAX);
    }

    #[test]
    fn a_frozen_height_pins_both_bounds_for_that_axis() {
        let mut constraints = LearnedConstraints::default();
        constraints.freeze_height(500.0);
        assert_eq!(constraints.min, Some(Size::new(0.0, 500.0)));
        let max = constraints.max.expect("max");
        assert_eq!(max.width, f64::MAX);
        assert_eq!(max.height, 500.0);
    }
}
