// SPDX-License-Identifier: GPL-3.0-or-later
//! The small platform interface the daemon talks to.
//!
//! Everything the event loop does to macOS goes through [`PlatformBackend`].
//! The real implementation ([`crate::macos::MacPlatform`]) is a thin adapter
//! over `cosmic-mac-platform`; the unit tests use a recording mock so the
//! command mapping is exhaustively testable on the host.

use std::sync::Arc;

use cosmic_mac_protocol::{Rect, StackInfo, ToplevelInfo, WindowId};

/// Result type used across the backend boundary.  The daemon logs failures and
/// keeps running; one closed window must never stop the event loop.
pub type PlatformResult<T> = Result<T, String>;

/// Platform operations needed by the window manager.
///
/// All methods must be safe to call from any thread (the AX and SkyLight
/// primitives are).  The overlay implementation owns its own thread.
pub trait PlatformBackend: Send + Sync {
    /// Move and resize a window.
    fn set_frame(&self, wid: WindowId, rect: Rect) -> PlatformResult<()>;
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
