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
/// The platform's `probe_size_constraints` discovers minima by momentarily
/// shrinking the window, reads declared maxima from `AXMaxSize`, and classifies
/// each axis as free, minimum-bounded or frozen.  A frozen axis is one the
/// application refuses to change at all (System Settings freezes its width).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SizeConstraints {
    /// Smallest size the window accepts, if any axis has a minimum.
    pub min: Option<Size>,
    /// Declared maximum size, if the application publishes one.
    pub max: Option<Size>,
    /// Value a frozen width is stuck at.
    pub width_frozen: Option<f64>,
    /// Value a frozen height is stuck at.
    pub height_frozen: Option<f64>,
}

/// The outcome of moving and resizing a window.
///
/// `applied` is what the window actually reached (read back from the
/// Accessibility API), and `clamped` is true when that differs from
/// `requested` by more than [`AppliedFrame::TOLERANCE`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppliedFrame {
    /// The frame the daemon asked for.
    pub requested: Rect,
    /// The frame the window ended up at.
    pub applied: Rect,
    /// True when the app refused part of the request.
    pub clamped: bool,
    /// True when the app ignored the width change entirely.
    pub width_frozen: bool,
    /// True when the app ignored the height change entirely.
    pub height_frozen: bool,
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
            width_frozen: false,
            height_frozen: false,
        }
    }

    /// Build a frame with per-axis outcomes (used by the real backend).
    pub fn with_axes(
        requested: Rect,
        applied: Rect,
        width_frozen: bool,
        height_frozen: bool,
    ) -> Self {
        AppliedFrame {
            width_frozen,
            height_frozen,
            ..AppliedFrame::new(requested, applied)
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
