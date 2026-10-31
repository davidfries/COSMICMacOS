// SPDX-License-Identifier: GPL-3.0-or-later
//! The small platform interface the daemon talks to.
//!
//! Everything the event loop does to macOS goes through [`PlatformBackend`].
//! The real implementation ([`crate::macos::MacPlatform`]) is a thin adapter
//! over `cosmic-mac-platform`; the unit tests use a recording mock so the
//! command mapping is exhaustively testable on the host.
//!
//! # Honest frames
//!
//! Native apps routinely refuse an Accessibility resize: `AXUIElementSetAttributeValue`
//! returns success while AppKit clamps the window to its minimum size, snaps it to
//! a resize grid, or freezes a dimension outright (System Settings freezes its
//! width).  [`PlatformBackend::set_frame`] therefore returns an [`AppliedFrame`]
//! carrying the frame that was actually observed read back from the window, not
//! just the frame that was requested.  The daemon uses the difference to learn
//! per-window size constraints (see `crate::core`) instead of pretending the
//! layout was applied.

use std::sync::Arc;

use cosmic_mac_protocol::{Rect, Size, StackInfo, ToplevelInfo, WindowId};

/// Result type used across the backend boundary.  The daemon logs failures and
/// keeps running; one closed window must never stop the event loop.
pub type PlatformResult<T> = Result<T, String>;

/// Per-window size constraints reported by the platform probe.
///
/// The platform's `probe_size_constraints` performs a shrink **and** a grow check,
/// reads declared maxima from `AXMaxSize`, and classifies each axis as free,
/// minimum-bounded or genuinely frozen.  `min` and `max` already encode that
/// classification: a `Min` axis leaves `max` unbounded (`f64::MAX` on that axis)
/// unless the grow probe also found a real maximum, while a genuinely frozen
/// axis contributes the same value to both `min` and `max`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SizeConstraints {
    /// Smallest size the window accepts, per axis (`0.0` means "no minimum").
    pub min: Option<Size>,
    /// Largest size the window accepts, per axis (`f64::MAX` means "no maximum").
    /// `None` only when no axis has a maximum.
    pub max: Option<Size>,
}

/// The outcome of moving and resizing a window.
///
/// `applied` is what the window actually reached (read back from the
/// Accessibility API), and `clamped` is true when that differs from
/// `requested` by more than [`AppliedFrame::TOLERANCE`].  A single operation
/// cannot tell a minimum, a maximum or a genuinely frozen dimension apart;
/// only the direction of the difference is meaningful here.  Use
/// [`PlatformBackend::size_constraints`] for the authoritative classification.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppliedFrame {
    /// The frame the daemon asked for.
    pub requested: Rect,
    /// The frame the window ended up at.
    pub applied: Rect,
    /// True when the app refused part of the request.
    pub clamped: bool,
}

impl AppliedFrame {
    /// Differences of a pixel or less are AX/AX rounding, not a clamp.
    pub const TOLERANCE: f64 = 1.0;

    /// Compare the request with the read-back frame.
    pub fn new(requested: Rect, applied: Rect) -> Self {
        AppliedFrame {
            requested,
            applied,
            clamped: !applied.approx_eq(requested, Self::TOLERANCE),
        }
    }

    /// True when the window reached `tile` (within the tolerance).
    pub fn reached(self, tile: Rect) -> bool {
        self.applied.approx_eq(tile, Self::TOLERANCE)
    }

    /// True when the size differs from the request by more than `tolerance`.
    ///
    /// Only a size difference can be explained by a minimum/maximum constraint;
    /// a position-only difference (an edge clamp) is not worth a size probe.
    pub fn size_differs(self, tolerance: f64) -> bool {
        (self.applied.width - self.requested.width).abs() > tolerance
            || (self.applied.height - self.requested.height).abs() > tolerance
    }

    /// Width difference of the clamped result (0 when the width was honoured).
    pub fn width_shortfall(self) -> f64 {
        (self.applied.width - self.requested.width).abs()
    }

    /// Height difference of the clamped result (0 when the height was honoured).
    pub fn height_shortfall(self) -> f64 {
        (self.applied.height - self.requested.height).abs()
    }
}

/// Platform operations needed by the window manager.
///
/// All methods must be safe to call from any thread (the AX and SkyLight
/// primitives are).  The overlay implementation owns its own thread.
pub trait PlatformBackend: Send + Sync {
    /// Move and resize a window, returning the frame it actually reached.
    fn set_frame(&self, wid: WindowId, rect: Rect) -> PlatformResult<AppliedFrame>;
    /// Probe a window's size constraints.
    ///
    /// This is an *active probe* (shrink the window, read it back, restore it),
    /// so it is expensive and visibly moves the window for a moment.  The
    /// platform caches the result per window; the daemon additionally only
    /// calls it when a frame application was clamped and remembers the answer
    /// for the rest of the session.
    fn size_constraints(&self, wid: WindowId) -> PlatformResult<Option<SizeConstraints>>;
    /// Drop the platform's cached probe result for `wid`.
    ///
    /// Called when the user resizes a tiled window: the application's current
    /// constraints may have changed, so the next clamp must re-probe instead of
    /// reusing a stale minimum/maximum.
    fn forget_size_constraints(&self, _wid: WindowId) {}
    /// Raise a window without changing focus.
    fn raise(&self, wid: WindowId) -> PlatformResult<()>;
    /// Focus a window without raising it.
    fn focus(&self, wid: WindowId) -> PlatformResult<()>;
    /// Close a window through its AX close button.
    fn close(&self, wid: WindowId) -> PlatformResult<()>;
    /// Park a window at the bottom-right corner of `display_frame`.
    fn park(&self, wid: WindowId, display_frame: Rect) -> PlatformResult<Option<Rect>>;
    /// Restore a parked window; returns the saved frame when one existed.
    fn unpark(&self, wid: WindowId) -> PlatformResult<Option<Rect>>;
    /// Saved frame of a parked window, if any.
    fn saved_frame(&self, wid: WindowId) -> Option<Rect>;
    /// Every parked window and its saved frame.
    fn parked(&self) -> Vec<(WindowId, Rect)>;
    /// Forget a parked window without moving it.
    fn forget(&self, wid: WindowId);
    /// Restore every parked window (errors are per window).
    fn restore_all(&self) -> Vec<(WindowId, String)>;
    /// Current frame of a window, if it still exists.
    fn frame(&self, wid: WindowId) -> Option<Rect>;
    /// Refresh one window's metadata.
    fn window_info(&self, wid: WindowId) -> Option<ToplevelInfo>;
    /// Every on-screen window, front-to-back.
    fn windows(&self) -> Vec<ToplevelInfo>;
    /// Show, update or hide the stack tab bar.
    fn set_stack_bar(&self, stack: Option<StackInfo>) -> PlatformResult<()>;
    /// Work area of the main display (top-left global coordinates).
    fn main_work_area(&self) -> Rect;
    /// Full frame of the display containing `wid`, for corner parking.
    fn display_frame(&self, wid: WindowId) -> Rect;
}

/// Convenience alias for the shared backend handle.
pub type SharedBackend = Arc<dyn PlatformBackend>;
