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
//! A single clamp is not a bound either, not even after a successful probe.
//! Apps with a resize grid (Terminal and iTerm snap to their character cell)
//! answer almost every request a few pixels off, in either direction: a
//! 900×500 tile comes back as 896×504.  Promoting those candidates pinned
//! Terminal at a snapped maximum and centred it with gaps for the session.  A
//! candidate is therefore promoted only once the probe has succeeded **and**
//!
//! * the probe explains it: the probe confirmed a bound in the same direction
//!   on that axis, the request lay beyond that bound (so a refusal was
//!   expected), and the app answered within [`SNAP_TOLERANCE`] of it; or
//! * it repeated: a second clamp, from a request more than [`SNAP_TOLERANCE`]
//!   away from the first, produced the same value.  A real bound stays put
//!   when the request moves; a grid snap moves with it.
//!
//! Confirmed constraints are cached for the rest of the session (or until the
//! user resizes the window, which invalidates the cache and the platform probe).

use cosmic_mac_protocol::{Rect, Size, ToplevelInfo};

use crate::backend::AppliedFrame;

/// Largest request/result difference that resize-grid snapping can explain,
/// in logical pixels.
///
/// Terminal and iTerm round every size to their character cell, so a request
/// comes back up to one cell away (Terminal at its default font: 7 px wide and
/// 14 px high cells; common font sizes stay under ~20 px).  24 px covers that
/// with headroom while staying well below half of the platform probe's 128 px
/// grow step, the shortfall beyond which the probe itself calls a maximum.
///
/// It is used twice:
///
/// * two clamps only count as the same bound seen twice when their requests
///   are more than this far apart, so both cannot fall into one grid cell;
/// * a clamp the probe explains may refine the probe's bound by at most this
///   much, so a stale or lagging read-back cannot move it far.
pub const SNAP_TOLERANCE: f64 = 24.0;

/// Which side of the reachable range a candidate bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// The app refused to shrink to the request.
    Min,
    /// The app refused to grow to the request.
    Max,
}

/// One directional bound candidate on one axis: what the app produced instead
/// of the request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// The size the app produced, in logical pixels.
    pub value: f64,
    /// The size the daemon had asked for when `value` was first observed.
    pub requested: f64,
    /// Whether a later clamp, from a request more than [`SNAP_TOLERANCE`] away
    /// from `requested`, produced the same value.
    pub repeated: bool,
}

impl Candidate {
    /// Merge one clamp into `slot`.
    ///
    /// The same value (within [`AppliedFrame::TOLERANCE`]) from a clearly
    /// different request marks the candidate as repeated.  The same value from
    /// (nearly) the same request proves nothing new: a re-tile re-applies the
    /// same tile, and two nearby requests can round to the same grid cell.  A
    /// different value replaces the candidate, because a bound that moves with
    /// the request was a snap.
    fn observe(slot: &mut Option<Candidate>, requested: f64, applied: f64) {
        match slot {
            Some(candidate) if (candidate.value - applied).abs() <= AppliedFrame::TOLERANCE => {
                if (candidate.requested - requested).abs() > SNAP_TOLERANCE {
                    candidate.repeated = true;
                }
            }
            _ => {
                *slot = Some(Candidate {
                    value: applied,
                    requested,
                    repeated: false,
                });
            }
        }
    }

    /// Whether this candidate may become an authoritative bound, given the
    /// bound the probe confirmed in the same direction on the same axis.
    fn promotable(self, probed: Option<f64>, direction: Direction) -> bool {
        if self.repeated {
            return true;
        }
        let Some(bound) = probed else {
            return false;
        };
        let tolerance = AppliedFrame::TOLERANCE;
        let refusal_expected = match direction {
            Direction::Min => self.requested <= bound + tolerance,
            Direction::Max => self.requested >= bound - tolerance,
        };
        refusal_expected && (self.value - bound).abs() <= SNAP_TOLERANCE
    }
}

/// Candidate bounds in one direction (minimum or maximum), per axis.
///
/// Each axis keeps the latest clamp in that direction; `None` means "no
/// evidence on this axis".  Candidates only become authoritative through
/// [`LearnedConstraints::promote_candidates`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AxisBounds {
    width: Option<Candidate>,
    height: Option<Candidate>,
}

impl AxisBounds {
    /// Candidate width, in logical pixels.
    pub fn width(self) -> Option<f64> {
        self.width.map(|candidate| candidate.value)
    }

    /// Candidate height, in logical pixels.
    pub fn height(self) -> Option<f64> {
        self.height.map(|candidate| candidate.value)
    }

    /// Merge one clamp; each axis is a `(requested, applied)` pair.
    fn observe(&mut self, width: Option<(f64, f64)>, height: Option<(f64, f64)>) {
        if let Some((requested, applied)) = width {
            Candidate::observe(&mut self.width, requested, applied);
        }
        if let Some((requested, applied)) = height {
            Candidate::observe(&mut self.height, requested, applied);
        }
    }

    /// The promotable candidates as a protocol size: `0.0` on an axis without
    /// a minimum, `f64::MAX` on an axis without a maximum.  `None` when no
    /// axis qualifies.
    fn promotable(self, probed: Option<Size>, direction: Direction) -> Option<Size> {
        let width = self.width.filter(|candidate| {
            candidate.promotable(
                probe_bound(probed.map(|size| size.width), direction),
                direction,
            )
        });
        let height = self.height.filter(|candidate| {
            candidate.promotable(
                probe_bound(probed.map(|size| size.height), direction),
                direction,
            )
        });
        let free = match direction {
            Direction::Min => 0.0,
            Direction::Max => f64::MAX,
        };
        (width.is_some() || height.is_some()).then(|| {
            Size::new(
                width.map_or(free, |candidate| candidate.value),
                height.map_or(free, |candidate| candidate.value),
            )
        })
    }
}

/// The probe's bound on one axis, or `None` when the probe found the axis free
/// in that direction (`0.0` minimum, unbounded maximum).
fn probe_bound(value: Option<f64>, direction: Direction) -> Option<f64> {
    value.filter(|value| match direction {
        Direction::Min => *value > 0.0,
        Direction::Max => *value < f64::MAX / 2.0,
    })
}

/// What the daemon has learned about one window's reachable geometry.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LearnedConstraints {
    /// Confirmed smallest size the window accepts.
    pub min: Option<Size>,
    /// Confirmed largest size the window accepts.  Axes without a learned
    /// maximum stay [`Size::UNBOUNDED`] so the layout treats them as free.
    pub max: Option<Size>,
    /// Minimum reported by the probe itself, before any candidate was
    /// promoted.  With [`Self::probe_max`] this is the probe-confirmed range
    /// that can explain a clamp.
    pub probe_min: Option<Size>,
    /// Maximum reported by the probe itself (see [`Self::probe_min`]).
    pub probe_max: Option<Size>,
    /// Directional lower-bound candidates from clamped frames.
    pub candidate_min: AxisBounds,
    /// Directional upper-bound candidates from clamped frames.
    pub candidate_max: AxisBounds,
    /// Whether the probe is finished for this epoch: it answered, reported
    /// nothing, or failed transiently [`Self::MAX_PROBE_ATTEMPTS`] times.
    pub probed: bool,
    /// Transient probe failures so far; reset when the entry is invalidated.
    pub probe_attempts: u8,
    /// Whether the probe produced an authoritative answer.
    pub confirmed: bool,
    /// Whether the "cannot reach its tile" warning was logged for the width.
    pub warned_width: bool,
    /// Whether the "cannot reach its tile" warning was logged for the height.
    pub warned_height: bool,
}

impl LearnedConstraints {
    /// Transient probe failures tolerated before the daemon gives up on a window.
    pub const MAX_PROBE_ATTEMPTS: u8 = 3;

    /// Record the direction of a clamped frame.
    ///
    /// * `applied > requested` → the app refused to shrink: a **minimum** candidate.
    /// * `applied < requested` → the app refused to grow: a **maximum** candidate.
    ///
    /// This deliberately never sets both bounds.  A single observation cannot
    /// distinguish a minimum from a genuinely frozen dimension; only the probe
    /// can.  Nor is it a bound by itself: see [`Self::promote_candidates`].
    pub fn note_frame(&mut self, requested: Rect, applied: Rect) {
        let tolerance = AppliedFrame::TOLERANCE;
        let (mut min_width, mut min_height) = (None, None);
        let (mut max_width, mut max_height) = (None, None);

        if applied.width > requested.width + tolerance {
            min_width = Some((requested.width, applied.width));
        } else if applied.width < requested.width - tolerance {
            max_width = Some((requested.width, applied.width));
        }
        if applied.height > requested.height + tolerance {
            min_height = Some((requested.height, applied.height));
        } else if applied.height < requested.height - tolerance {
            max_height = Some((requested.height, applied.height));
        }

        self.candidate_min.observe(min_width, min_height);
        self.candidate_max.observe(max_width, max_height);
    }

    /// Merge an authoritative probe result and promote the candidates it
    /// explains (or that already repeated).
    pub fn confirm_probe(&mut self, min: Option<Size>, max: Option<Size>) {
        if let Some(min) = min {
            self.probe_min = Some(strongest_size(self.probe_min, min));
            self.min = Some(strongest_size(self.min, min));
        }
        if let Some(max) = max {
            self.probe_max = Some(weakest_size(self.probe_max, max));
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

    /// Record a transient probe failure (an Accessibility error, as opposed to
    /// a definitive "nothing reported").
    ///
    /// The window stays unprobed so the next clamp tries again, until
    /// [`Self::MAX_PROBE_ATTEMPTS`] failures have accumulated; then the probe is
    /// marked finished (still unconfirmed).  Returns `true` when this call gave up.
    pub fn defer_probe(&mut self) -> bool {
        self.probe_attempts = self.probe_attempts.saturating_add(1);
        if self.probe_attempts >= Self::MAX_PROBE_ATTEMPTS {
            self.probed = true;
            true
        } else {
            false
        }
    }

    /// Promote the candidates that are trustworthy once the probe has succeeded.
    ///
    /// Called when the probe confirms and again after every later clamp.  A
    /// candidate qualifies only when the probe explains it or it repeated (see
    /// the module docs); a grid snap does neither, so Terminal keeps the
    /// probe's bounds however many requests it rounds.  Merging keeps the
    /// strongest minimum and the weakest maximum.
    pub fn promote_candidates(&mut self) {
        if !self.confirmed {
            return;
        }
        if let Some(min) = self
            .candidate_min
            .promotable(self.probe_min, Direction::Min)
        {
            self.min = Some(strongest_size(self.min, min));
        }
        if let Some(max) = self
            .candidate_max
            .promotable(self.probe_max, Direction::Max)
        {
            self.max = Some(weakest_size(self.max, max));
        }
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
        assert_eq!(constraints.candidate_min.width(), Some(642.0));
        assert_eq!(constraints.candidate_min.height(), None);
        assert!(constraints.candidate_max.width().is_none());
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
        assert_eq!(constraints.candidate_max.width(), Some(400.0));
        assert!(constraints.candidate_min.width().is_none());
        assert!(constraints.min.is_none());
        assert!(constraints.max.is_none());
    }

    #[test]
    fn probe_confirmation_promotes_a_candidate_it_explains_and_never_invents_a_max() {
        // The request lay below the probe's 640px minimum and the app answered
        // within a cell of it: that is the minimum as reached at the tile.
        let mut constraints = clamp(
            Rect::new(0.0, 0.0, 500.0, 400.0),
            Rect::new(0.0, 0.0, 642.0, 400.0),
        );
        constraints.confirm_probe(Some(Size::new(640.0, 100.0)), None);
        assert_eq!(constraints.min, Some(Size::new(642.0, 100.0)));
        assert_eq!(constraints.max, None, "a Min must leave the axis growable");
        assert!(constraints.confirmed);
    }

    #[test]
    fn a_candidate_far_from_the_probe_bound_waits_for_a_repeat() {
        let mut constraints = clamp(
            Rect::new(0.0, 0.0, 500.0, 400.0),
            Rect::new(0.0, 0.0, 642.0, 400.0),
        );
        constraints.confirm_probe(Some(Size::new(600.0, 100.0)), None);
        assert_eq!(
            constraints.min,
            Some(Size::new(600.0, 100.0)),
            "one clamp the probe contradicts must not override it"
        );

        // The same 642px from a clearly different request is a real bound.
        constraints.note_frame(
            Rect::new(0.0, 0.0, 400.0, 400.0),
            Rect::new(0.0, 0.0, 642.0, 400.0),
        );
        constraints.promote_candidates();
        assert_eq!(constraints.min, Some(Size::new(642.0, 100.0)));
    }

    #[test]
    fn the_probe_only_explains_clamps_of_requests_beyond_its_bound() {
        let mut constraints = LearnedConstraints::default();
        constraints.confirm_probe(
            Some(Size::new(140.0, 70.0)),
            Some(Size::new(1200.0, f64::MAX)),
        );
        // 80px lies above the 70px minimum, so rounding it up to 84 is a snap.
        constraints.note_frame(
            Rect::new(0.0, 0.0, 300.0, 80.0),
            Rect::new(0.0, 0.0, 300.0, 84.0),
        );
        // 1190px lies below the 1200px maximum, so 1183 is a snap too.
        constraints.note_frame(
            Rect::new(0.0, 0.0, 1190.0, 300.0),
            Rect::new(0.0, 0.0, 1183.0, 300.0),
        );
        constraints.promote_candidates();
        assert_eq!(constraints.min, Some(Size::new(140.0, 70.0)));
        assert_eq!(constraints.max, Some(Size::new(1200.0, f64::MAX)));

        // A request below the minimum answered far from it (a stale read-back)
        // is not explained either.
        constraints.note_frame(
            Rect::new(0.0, 0.0, 100.0, 300.0),
            Rect::new(0.0, 0.0, 500.0, 300.0),
        );
        constraints.promote_candidates();
        assert_eq!(constraints.min, Some(Size::new(140.0, 70.0)));

        // Requests beyond the maximum answered within a cell of it refine it.
        constraints.note_frame(
            Rect::new(0.0, 0.0, 1300.0, 300.0),
            Rect::new(0.0, 0.0, 1197.0, 300.0),
        );
        constraints.promote_candidates();
        assert_eq!(constraints.max, Some(Size::new(1197.0, f64::MAX)));
    }

    /// Terminal-style resize grid: widths round down to 7px cells, heights to
    /// the nearest 14px cell, so a 900×500 tile comes back as 896×504.
    fn terminal(requested: Rect) -> Rect {
        Rect::new(
            requested.x,
            requested.y,
            (requested.width / 7.0).floor() * 7.0,
            (requested.height / 14.0).round() * 14.0,
        )
    }

    #[test]
    fn a_grid_snapping_app_never_gets_a_bound_from_its_snaps() {
        let first = Rect::new(0.0, 0.0, 900.0, 500.0);
        assert_eq!(terminal(first), Rect::new(0.0, 0.0, 896.0, 504.0));
        // The first clamp lands before the probe, which then reports
        // Terminal's real minimum and no maximum.
        let mut constraints = clamp(first, terminal(first));
        constraints.confirm_probe(Some(Size::new(140.0, 70.0)), None);
        assert_eq!(constraints.min, Some(Size::new(140.0, 70.0)));
        assert_eq!(constraints.max, None);

        // Re-tiles repeat the same request, nearby requests fall into the same
        // cell, and layout changes ask for different sizes.
        for (width, height) in [
            (900.0, 500.0),
            (950.0, 520.0),
            (900.0, 500.0),
            (902.0, 497.0),
            (1000.0, 560.0),
            (900.0, 500.0),
            (640.0, 380.0),
        ] {
            let requested = Rect::new(0.0, 0.0, width, height);
            constraints.note_frame(requested, terminal(requested));
            constraints.promote_candidates();
        }
        assert_eq!(
            constraints.min,
            Some(Size::new(140.0, 70.0)),
            "a snap must not raise the minimum"
        );
        assert_eq!(constraints.max, None, "a snap must not pin a maximum");
    }

    #[test]
    fn a_real_maximum_seen_twice_from_different_requests_is_promoted() {
        let mut constraints = LearnedConstraints::default();
        // The probe succeeded but found no maximum on this axis.
        constraints.confirm_probe(None, None);
        let capped = Rect::new(0.0, 0.0, 800.0, 600.0);

        constraints.note_frame(Rect::new(0.0, 0.0, 900.0, 600.0), capped);
        constraints.promote_candidates();
        assert_eq!(constraints.max, None, "one clamp is not a bound");
        // A re-tile repeats the same request: still one observation.
        constraints.note_frame(Rect::new(0.0, 0.0, 900.0, 600.0), capped);
        constraints.promote_candidates();
        assert_eq!(constraints.max, None);

        constraints.note_frame(Rect::new(0.0, 0.0, 1000.0, 600.0), capped);
        constraints.promote_candidates();
        assert_eq!(constraints.max, Some(Size::new(800.0, f64::MAX)));
        assert_eq!(constraints.min, None);
    }

    #[test]
    fn the_same_value_only_repeats_from_requests_more_than_a_snap_apart() {
        let mut constraints = LearnedConstraints::default();
        constraints.confirm_probe(None, None);
        let capped = Rect::new(0.0, 0.0, 896.0, 600.0);

        constraints.note_frame(Rect::new(0.0, 0.0, 900.0, 600.0), capped);
        constraints.note_frame(Rect::new(0.0, 0.0, 900.0 + SNAP_TOLERANCE, 600.0), capped);
        constraints.promote_candidates();
        assert_eq!(constraints.max, None, "both requests fit one grid cell");
        assert!(!constraints.candidate_max.width.unwrap().repeated);

        constraints.note_frame(Rect::new(0.0, 0.0, 901.0 + SNAP_TOLERANCE, 600.0), capped);
        constraints.promote_candidates();
        assert_eq!(constraints.max, Some(Size::new(896.0, f64::MAX)));
    }

    #[test]
    fn a_different_value_replaces_the_candidate() {
        let mut constraints = LearnedConstraints::default();
        constraints.confirm_probe(None, None);
        constraints.note_frame(
            Rect::new(0.0, 0.0, 900.0, 600.0),
            Rect::new(0.0, 0.0, 800.0, 600.0),
        );
        constraints.note_frame(
            Rect::new(0.0, 0.0, 1000.0, 600.0),
            Rect::new(0.0, 0.0, 850.0, 600.0),
        );
        // 800 then 850: the value moved with the request, nothing repeated.
        constraints.note_frame(
            Rect::new(0.0, 0.0, 1100.0, 600.0),
            Rect::new(0.0, 0.0, 800.0, 600.0),
        );
        constraints.promote_candidates();
        assert_eq!(constraints.max, None);
        assert_eq!(constraints.candidate_max.width(), Some(800.0));
    }

    #[test]
    fn repetition_before_the_probe_is_promoted_once_the_probe_succeeds() {
        let mut constraints = LearnedConstraints::default();
        let capped = Rect::new(0.0, 0.0, 800.0, 600.0);
        constraints.note_frame(Rect::new(0.0, 0.0, 900.0, 600.0), capped);
        constraints.note_frame(Rect::new(0.0, 0.0, 1000.0, 600.0), capped);
        constraints.promote_candidates();
        assert_eq!(
            constraints.max, None,
            "nothing is promoted before the probe"
        );

        constraints.confirm_probe(None, None);
        assert_eq!(constraints.max, Some(Size::new(800.0, f64::MAX)));
    }

    #[test]
    fn a_probe_failure_does_not_promote_candidates() {
        let mut constraints = clamp(
            Rect::new(0.0, 0.0, 500.0, 400.0),
            Rect::new(0.0, 0.0, 642.0, 400.0),
        );
        // Even a repeated candidate needs the probe's confirmation.
        constraints.note_frame(
            Rect::new(0.0, 0.0, 400.0, 400.0),
            Rect::new(0.0, 0.0, 642.0, 400.0),
        );
        constraints.fail_probe();
        constraints.promote_candidates();
        assert!(constraints.probed);
        assert!(!constraints.confirmed);
        assert!(constraints.min.is_none());
        assert!(constraints.max.is_none());
        assert_eq!(constraints.candidate_min.width(), Some(642.0));
    }

    #[test]
    fn defer_probe_counts_attempts_and_keeps_the_probe_open_below_the_cap() {
        let mut constraints = LearnedConstraints::default();
        for attempt in 1..LearnedConstraints::MAX_PROBE_ATTEMPTS {
            assert!(!constraints.defer_probe());
            assert_eq!(constraints.probe_attempts, attempt);
            assert!(!constraints.probed, "attempt {attempt} must stay retryable");
            assert!(!constraints.confirmed);
        }
    }

    #[test]
    fn defer_probe_gives_up_at_the_cap_without_confirming() {
        let mut constraints = LearnedConstraints::default();
        let mut gave_up = false;
        for _ in 0..LearnedConstraints::MAX_PROBE_ATTEMPTS {
            gave_up = constraints.defer_probe();
        }
        assert!(gave_up, "the capping call reports that it gave up");
        assert_eq!(
            constraints.probe_attempts,
            LearnedConstraints::MAX_PROBE_ATTEMPTS
        );
        assert!(constraints.probed);
        assert!(!constraints.confirmed);
    }

    #[test]
    fn defer_probe_never_promotes_candidates() {
        let mut constraints = clamp(
            Rect::new(0.0, 0.0, 500.0, 400.0),
            Rect::new(0.0, 0.0, 642.0, 400.0),
        );
        for _ in 0..=LearnedConstraints::MAX_PROBE_ATTEMPTS {
            constraints.defer_probe();
            constraints.promote_candidates();
        }
        assert!(!constraints.confirmed);
        assert!(constraints.min.is_none());
        assert!(constraints.max.is_none());
        assert_eq!(constraints.candidate_min.width(), Some(642.0));
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
