// SPDX-License-Identifier: GPL-3.0-or-later
//! AppKit tweaks that turn the winit-created `NSWindow` into a panel.
//!
//! libcosmic/iced do not expose macOS window levels beyond winit's
//! `AlwaysOnTop` (`NSFloatingWindowLevel`), Spaces collection behaviour or
//! absolute positioning, so the panel reaches the `NSWindow` directly: iced's
//! `window::run_with_handle` hands over the raw-window-handle, whose AppKit
//! variant carries the content `NSView`; its `window` is the `NSWindow`.
//! The callback runs on the winit event-loop thread, which is the main thread
//! on macOS, so AppKit calls are allowed there.

use cosmic::iced::window::raw_window_handle::{RawWindowHandle, WindowHandle};
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSScreen, NSStatusWindowLevel, NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::PANEL_HEIGHT;

/// What the panel `NSWindow` looks like after [`configure_panel_window`].
#[derive(Clone, Debug)]
pub struct PanelWindow {
    /// Window-server window number (the CG/SLS window id).
    pub window_number: isize,
    /// Frame in AppKit screen coordinates (bottom-left origin): x, y, w, h.
    pub frame: (f64, f64, f64, f64),
    pub level: isize,
    pub style_mask: usize,
    pub collection_behavior: usize,
}

/// Makes the window a borderless, status-level bar on every Space, stretched
/// across the top edge of the main screen.
///
/// Signature matches iced's `window::run_with_handle` callback.
pub fn configure_panel_window(handle: WindowHandle<'_>) -> Result<PanelWindow, String> {
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return Err(format!(
            "not an AppKit window handle: {:?}",
            handle.as_raw()
        ));
    };
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "window-handle callback is not on the main thread".to_owned())?;

    // SAFETY: raw-window-handle guarantees `ns_view` points at a live NSView
    // for as long as the borrowed handle exists, and we are on the main
    // thread (checked above), which AppKit requires.
    let view: &NSView = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
    let window = view
        .window()
        .ok_or_else(|| "content NSView is not attached to an NSWindow".to_owned())?;
    let screen = NSScreen::mainScreen(mtm)
        .ok_or_else(|| "no main screen (is the display asleep?)".to_owned())?;

    window.setStyleMask(NSWindowStyleMask::Borderless);
    window.setLevel(NSStatusWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::Stationary,
    );

    // AppKit frames are bottom-left based: the top edge of the screen is
    // `origin.y + height`. The full screen frame (not `visibleFrame`) puts
    // the bar over the menu-bar area; status level sits above the menu bar.
    let screen_frame = screen.frame();
    let frame = NSRect::new(
        NSPoint::new(
            screen_frame.origin.x,
            screen_frame.origin.y + screen_frame.size.height - PANEL_HEIGHT,
        ),
        NSSize::new(screen_frame.size.width, PANEL_HEIGHT),
    );
    window.setFrame_display(frame, true);

    let applied = window.frame();
    Ok(PanelWindow {
        window_number: window.windowNumber(),
        frame: (
            applied.origin.x,
            applied.origin.y,
            applied.size.width,
            applied.size.height,
        ),
        level: window.level(),
        style_mask: window.styleMask().0,
        collection_behavior: window.collectionBehavior().0,
    })
}
