//! macOS platform primitives for cosmicMac.
//!
//! This crate is deliberately free of layout or IPC types. It exposes:
//!
//! * [`window`]: enumeration (CGWindowList + AX), geometry operations, raise,
//!   focus-without-raise, minimize/close and the [`ParkStore`] used for virtual
//!   workspaces and stacks.
//! * [`observers`]: SkyLight + AX event sources joined into [`PlatformEvent`].
//! * [`display`]: display enumeration (work areas from `NSScreen.visibleFrame`)
//!   and display-change callbacks.
//! * [`coords`]: AX/SLS top-left global vs AppKit bottom-left conversion.
//! * [`eventtap`]: the global keyboard tap, including re-enable on timeout, a
//!   listen-only left-mouse tap and an active left-click tap.
//! * [`overlay`]: raw SkyLight overlay windows ordered above a foreign window.
//! * [`tcc`]: Accessibility / Screen Recording presence checks.
//! * [`skylight`]: all private SkyLight FFI, loaded through `dlopen`.
//!
//! Everything is macOS-only. The crate is `unsafe`-heavy by nature; each raw
//! call documents its contract and the safe wrappers are the intended API.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]

#[cfg(not(target_os = "macos"))]
compile_error!("cosmic-mac-platform only supports macOS");

pub mod ax;
pub mod coords;
pub mod display;
pub mod error;
pub mod eventtap;
pub mod geometry;
pub mod observers;
pub mod overlay;
pub mod skylight;
pub mod tcc;
pub mod window;

pub use display::DisplayInfo;
pub use error::{PlatformError, Result};
pub use eventtap::{
    ClickTap, EventTap, HotkeySpec, KeyEvent, MouseEvent, MouseEventKind, MouseTap,
};
pub use geometry::{Point, Rect, Size};
pub use observers::{PlatformEvent, PlatformObserver};
pub use overlay::{Color, OverlayWindow};
pub use window::{
    AppliedFrame, AxWindow, AxisConstraint, DimensionOutcome, ParkStore, SizeConstraints,
    WindowInfo,
};
