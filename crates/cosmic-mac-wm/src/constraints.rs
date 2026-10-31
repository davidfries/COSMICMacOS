// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-window size constraints learned from the platform.
//!
//! macOS apps silently clamp Accessibility resizes.  When the daemon notices
//! that a window did not reach its tile, it records the **direction** of the
//! difference as a candidate (a shortfall on the shrink side is a minimum, a
//! shortfall on the grow side is a maximum) and confirms it with
//! [`crate::backend::PlatformBackend::size_constraints`], which runs the
//! platform's shrink **and** grow probe.
//!
//! A single `set_frame` outcome can never say that a dimension is frozen: it
//! only says that one request was ignored.  Notes, for example, refuses to
//! shrink below its natural 642px width but grows normally, so a shrink
//! rejection is a minimum, not a frozen dimension.  Only the probe's
//! `AxisConstraint::Frozen` result makes the minimum and maximum equal.
//!
//! Confirmed constraints are cached for the rest of the session (or until the
//! user resizes the window, which invalidates the cache and the platform probe).

use cosmic_mac_protocol::{Rect, Size, ToplevelInfo};

/// Candidate bounds for one axis, from clamp directions.
///
/// `None` means "no evidence on this axis".  Candidates are only promoted to
/// authoritative constraints after a successful probe.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AxisBounds {
    /// Lower bound candidate, in logical pixels.
    pub width: Option<f64>,
    /// Lower/upper bound candidate, in logical pixels.
    pub height: Option<f64>,
}

impl AxisBounds {
    /// Merge lower bounds: the strongest (largest) minimum wins.
    fn note_min(&mut self, width: Option<f64>, height: Option<f64>) {
        self.width = strongest(self.width, width);
        self.height = strongest(self.height, height);
    }

    /// Merge upper bounds: the weakest (smallest) maximum wins.
    fn note_max(&mut self, width: Option<f64>, height: Option<f64>) {
        self.width = weakest(self.width, width);
        self.height = weakest(self.height, height);
    }

    /// Turn the candidates into a protocol minimum (`0.0` on a free axis).
    fn to_min_size(self) -> Option<Size> {
        (self.width.is_some() || self.height.is_some())
            .then(|| Size::new(self.width.unwrap_or(0.0), self.height.unwrap_or(0.0)))
    }

    /// Turn the candidates into a protocol maximum (`f64::MAX` on a free axis).
    fn to_max_size(self) -> Option<Size> {
        (self.width.is_some() || self.height.is_some()).then(|| {
            Size::new(
                self.width.unwrap_or(f64::MAX),
                self.height.unwrap_or(f64::MAX),
            )
        })
    }
}

/// What the daemon has learned about one window's reachable geometry.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LearnedConstraints {
    /// Confirmed smallest size the window accepts.
    pub min: Option<Size>,
    /// Confirmed largest size the window accepts.  Axes without a learned
    /// maximum stay [`Size::UNBOUNDED`] so the layout treats them as free.
    pub max: Option<Size>,
    /// Directional lower-bound candidates from clamped frames.
    pub candidate_min: AxisBounds,
    /// Directional upper-bound candidates from clamped frames.
    pub candidate_max: AxisBounds,
    /// Whether the probe already ran (successfully or not) for this epoch.
    pub probed: bool,
    /// Whether the probe produced an authoritative answer.
    pub confirmed: bool,
    /// Whether the "cannot reach its tile" warning was logged for the width.
    pub warned_width: bool,
    /// Whether the "cannot reach its tile" warning was logged for the height.
    pub warned_height: bool,
}

impl LearnedConstraints {
    /// Record the direction of a clamped frame.
    ///
    /// * `applied > requested` → the app refused to shrink: a **minimum** candidate.
    /// * `applied < requested` → the app refused to grow: a **maximum** candidate.
    ///
    /// This deliberately never sets both bounds.  A single observation cannot
    /// distinguish a minimum from a genuinely frozen dimension; only the probe
    /// can.
    pub fn note_frame(&mut self, requested: Rect, applied: Rect) {
        let tolerance = crate::backend::AppliedFrame::TOLERANCE;
        let (mut min_width, mut min_height) = (None, None);
        let (mut max_width, mut max_height) = (None, None);

        if applied.width > requested.width + tolerance {
            min_width = Some(applied.width);
        } else if applied.width < requested.width - tolerance {
            max_width = Some(applied.width);
        }
        if applied.height > requested.height + tolerance {
            min_height = Some(applied.height);
        } else if applied.height < requested.height - tolerance {
            max_height = Some(applied.height);
        }

        self.candidate_min.note_min(min_width, min_height);
        self.candidate_max.note_max(max_width, max_height);
    }

    /// Merge an authoritative probe result and promote the candidates that the
    /// probe has now confirmed.
    pub fn confirm_probe(&mut self, min: Option<Size>, max: Option<Size>) {
        if let Some(min) = min {
            self.min = Some(strongest_size(self.min, min));
        }
        if let Some(max) = max {
            self.max = Some(weakest_size(self.max, max));
        }
        self.probed = true;
        self.confirmed = true;
        self.promote_candidates();
    }

    /// Record that the probe could not produce an answer.
    ///
    /// Candidates stay as evidence but are **not** promoted: without the grow
    /// check the daemon cannot tell a minimum from a frozen dimension, and a
    /// wrong maximum would make the window permanently too small.
    pub fn fail_probe(&mut self) {
        self.probed = true;
        self.confirmed = false;
    }

    /// Promote directional candidates once the probe has confirmed them.
    ///
    /// Merging keeps the strongest minimum and the weakest maximum.  This is
    /// also called after later clamps so a grid snap discovered after the probe
    /// can tighten the learned bounds.
    pub fn promote_candidates(&mut self) {
        if !self.confirmed {
            return;
        }
        if let Some(min) = self.candidate_min.to_min_size() {
            self.min = Some(strongest_size(self.min, min));
        }
        if let Some(max) = self.candidate_max.to_max_size() {
            self.max = Some(weakest_size(self.max, max));
        }
    }
}

fn strongest(current: Option<f64>, candidate: Option<f64>) -> Option<f64> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.max(candidate)),
        (current, None) => current,
        (None, candidate) => candidate,
    }
}

fn weakest(current: Option<f64>, candidate: Option<f64>) -> Option<f64> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.min(candidate)),
        (current, None) => current,
        (None, candidate) => candidate,
    }
}

fn strongest_size(current: Option<Size>, value: Size) -> Size {
    match current {
        Some(current) => Size::new(
            current.width.max(value.width),
            current.height.max(value.height),
        ),
        None => value,
    }
}

fn weakest_size(current: Option<Size>, value: Size) -> Size {
    match current {
        Some(current) => Size::new(
            current.width.min(value.width),
            current.height.min(value.height),
        ),
        None => value,
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
    // Assign unconditionally: a window whose learned constraints were
    // invalidated (user resize) must not keep stale bounds that are still in
    // the layout's snapshot.
    info.min_size = constraints.min;
    info.max_size = constraints.max;
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

    fn clamp(requested: Rect, applied: Rect) -> LearnedConstraints {
        let mut constraints = LearnedConstraints::default();
        constraints.note_frame(requested, applied);
        constraints
    }

    #[test]
    fn defaults_have_no_constraints() {
        let constraints = LearnedConstraints::default();
        assert!(constraints.min.is_none());
        assert!(constraints.max.is_none());
        assert!(!constraints.probed);
        assert!(!constraints.confirmed);
    }

    #[test]
    fn attach_carries_min_and_max_into_the_layout_input() {
        let constraints = LearnedConstraints {
            min: Some(Size::new(400.0, 300.0)),
            max: Some(Size::new(600.0, 500.0)),
            probed: true,
            confirmed: true,
            ..LearnedConstraints::default()
        };
        let attached = attach(info(), constraints);
        assert_eq!(attached.min_size, Some(Size::new(400.0, 300.0)));
        assert_eq!(attached.max_size, Some(Size::new(600.0, 500.0)));
        assert_eq!(attached.wid, 7);
    }

    #[test]
    fn attach_clears_stale_constraints_when_none_are_learned() {
        let stale = info()
            .with_min_size(Size::new(500.0, 500.0))
            .with_max_size(Size::new(600.0, 600.0));
        let attached = attach(stale, LearnedConstraints::default());
        assert!(attached.min_size.is_none());
        assert!(attached.max_size.is_none());
    }

    #[test]
    fn a_shrink_rejection_is_only_a_minimum_candidate() {
        let constraints = clamp(
            Rect::new(0.0, 0.0, 500.0, 400.0),
            Rect::new(0.0, 0.0, 642.0, 400.0),
        );
        assert_eq!(constraints.candidate_min.width, Some(642.0));
        assert_eq!(constraints.candidate_min.height, None);
        assert!(constraints.candidate_max.width.is_none());
        // A single observation must not pin both bounds.
        assert!(constraints.min.is_none());
        assert!(constraints.max.is_none());
    }

    #[test]
    fn a_grow_rejection_is_only_a_maximum_candidate() {
        let constraints = clamp(
            Rect::new(0.0, 0.0, 500.0, 400.0),
            Rect::new(0.0, 0.0, 400.0, 400.0),
        );
        assert_eq!(constraints.candidate_max.width, Some(400.0));
        assert!(constraints.candidate_min.width.is_none());
        assert!(constraints.min.is_none());
        assert!(constraints.max.is_none());
    }

    #[test]
    fn probe_confirmation_promotes_candidates_and_never_invents_a_max() {
        let mut constraints = clamp(
            Rect::new(0.0, 0.0, 500.0, 400.0),
            Rect::new(0.0, 0.0, 642.0, 400.0),
        );
        constraints.confirm_probe(Some(Size::new(600.0, 100.0)), None);
        assert_eq!(constraints.min, Some(Size::new(642.0, 100.0)));
        assert_eq!(constraints.max, None, "a Min must leave the axis growable");
        assert!(constraints.confirmed);
    }

    #[test]
    fn a_probe_failure_does_not_promote_candidates() {
        let mut constraints = clamp(
            Rect::new(0.0, 0.0, 500.0, 400.0),
            Rect::new(0.0, 0.0, 642.0, 400.0),
        );
        constraints.fail_probe();
        assert!(constraints.probed);
        assert!(!constraints.confirmed);
        assert!(constraints.min.is_none());
        assert!(constraints.max.is_none());
        assert_eq!(constraints.candidate_min.width, Some(642.0));
    }

    #[test]
    fn a_frozen_axis_from_the_probe_pins_both_bounds() {
        let mut constraints = LearnedConstraints::default();
        constraints.confirm_probe(
            Some(Size::new(715.0, 100.0)),
            Some(Size::new(715.0, f64::MAX)),
        );
        assert_eq!(constraints.min.map(|size| size.width), Some(715.0));
        assert_eq!(constraints.max.map(|size| size.width), Some(715.0));
        assert_eq!(constraints.max.map(|size| size.height), Some(f64::MAX));
    }

    #[test]
    fn merging_keeps_the_strongest_minimum_and_weakest_maximum() {
        let mut constraints = LearnedConstraints::default();
        constraints.confirm_probe(
            Some(Size::new(400.0, 200.0)),
            Some(Size::new(1000.0, 800.0)),
        );
        constraints.confirm_probe(Some(Size::new(500.0, 100.0)), Some(Size::new(900.0, 900.0)));
        assert_eq!(constraints.min, Some(Size::new(500.0, 200.0)));
        assert_eq!(constraints.max, Some(Size::new(900.0, 800.0)));
    }
}
