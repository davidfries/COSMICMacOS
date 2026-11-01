// SPDX-License-Identifier: GPL-3.0-or-later
//! AppKit glue: the launcher `NSWindow`, activation, and the side effects
//! of activating a result (launch, open URL, clipboard, focus via the WM).
//!
//! The window is reached like the Spike A panel did it: iced's
//! `window::run_with_handle` hands over the raw-window-handle whose AppKit
//! variant carries the content `NSView`; its `window` is winit's
//! `NSWindow`. The callback runs on the winit event-loop thread, which is
//! the main thread on macOS.
//!
//! Unlike the panel, the launcher must take keyboard focus:
//! - The binary embeds an `Info.plist` (`build.rs`) with a bundle
//!   identifier and `LSUIElement`, so winit treats it as bundled and leaves
//!   the activation policy alone instead of forcing `Regular` (Dock icon) at
//!   launch. [`configure_window`] still sets `Accessory` if the policy is
//!   anything else, then activates the app with
//!   `activateIgnoringOtherApps:` (the WM spawns the launcher from the
//!   background, so cooperative `activate` may be declined) and makes the
//!   window key. winit's window class overrides `canBecomeKeyWindow`, so a
//!   borderless window can be key.
//! - Level: `NSModalPanelWindowLevel` (8). The window manager ignores every
//!   non-zero level, and its stack tab bars copy their (level 0) target's
//!   level, so any non-zero level works for the WM; 8 additionally sits
//!   above other apps' floating palettes and "always on top" windows
//!   (`NSFloatingWindowLevel`, 3) while staying below the menu bar (24),
//!   status items (25) and pop-up menus (101), so system UI still draws over
//!   it. The window is created at winit's `AlwaysOnTop` (floating) level and
//!   hidden, so it is never visible at level 0.

use std::path::Path;

use cosmic::iced::window::raw_window_handle::{RawWindowHandle, WindowHandle};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSModalPanelWindowLevel, NSPasteboard,
    NSPasteboardTypeString, NSRunningApplication, NSScreen, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURL};

use crate::geometry::{self, Frame};
use crate::plugin::Action;
use crate::plugins::windows;

/// The launcher `NSWindow` after [`configure_window`], for the startup log.
#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub window_number: isize,
    /// Frame in AppKit screen coordinates (bottom-left origin).
    pub frame: Frame,
    /// The main screen's `visibleFrame` the window was centred in.
    pub visible_frame: Frame,
    pub level: isize,
    /// Activation policy before and after configuration.
    pub initial_policy: &'static str,
    pub policy: &'static str,
    /// Bundle identifier AppKit sees for the process (from the embedded
    /// `Info.plist`); `None` means winit treated the binary as unbundled.
    pub bundle_id: Option<String>,
    pub is_key: bool,
    pub is_active: bool,
}

fn window_from_handle(
    handle: WindowHandle<'_>,
) -> Result<(MainThreadMarker, Retained<NSWindow>), String> {
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
    Ok((mtm, window))
}

fn to_frame(rect: NSRect) -> Frame {
    Frame::new(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

/// Makes the launcher window a borderless, shadowless, modal-panel-level
/// window on every Space, centred on the main screen, and gives it keyboard
/// focus. Signature matches iced's `window::run_with_handle` callback.
pub fn configure_window(handle: WindowHandle<'_>) -> Result<WindowInfo, String> {
    let (mtm, window) = window_from_handle(handle)?;
    let screen = NSScreen::mainScreen(mtm)
        .ok_or_else(|| "no main screen (is the display asleep?)".to_owned())?;
    let app = NSApplication::sharedApplication(mtm);

    let initial_policy = app.activationPolicy();
    if initial_policy != NSApplicationActivationPolicy::Accessory
        && !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory)
    {
        log::warn!("setActivationPolicy(Accessory) was refused");
    }

    window.setStyleMask(NSWindowStyleMask::Borderless);
    window.setHasShadow(false);
    window.setLevel(NSModalPanelWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );

    // `visibleFrame` excludes the menu bar and the Dock; both frames are in
    // the bottom-left-origin global space, whatever the screen's size.
    let visible_frame = to_frame(screen.visibleFrame());
    let frame = geometry::centered_frame(
        visible_frame,
        f64::from(geometry::WINDOW_WIDTH),
        f64::from(geometry::window_height()),
    );
    window.setFrame_display(
        NSRect::new(
            NSPoint::new(frame.x, frame.y),
            NSSize::new(frame.width, frame.height),
        ),
        true,
    );

    activate(&app, &window);

    Ok(WindowInfo {
        window_number: window.windowNumber(),
        frame: to_frame(window.frame()),
        visible_frame,
        level: window.level(),
        initial_policy: policy_name(initial_policy),
        policy: policy_name(app.activationPolicy()),
        bundle_id: NSRunningApplication::currentApplication()
            .bundleIdentifier()
            .map(|id| id.to_string()),
        is_key: window.isKeyWindow(),
        is_active: app.isActive(),
    })
}

fn activate(app: &NSApplication, window: &NSWindow) {
    // The launcher is started by the WM (a background process), so the
    // cooperative `activate` could be declined; winit uses the same call.
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    window.makeKeyAndOrderFront(None);
}

/// Whether the launcher window is the key window. With `retry`, a window
/// that is not key is activated and made key once more (the result still
/// reports the state before the retry).
pub fn check_key(handle: WindowHandle<'_>, retry: bool) -> Result<bool, String> {
    let (mtm, window) = window_from_handle(handle)?;
    let is_key = window.isKeyWindow();
    if !is_key && retry {
        activate(&NSApplication::sharedApplication(mtm), &window);
    }
    Ok(is_key)
}

/// Hides the window immediately (before the process finishes exiting), so
/// focus moves to the launched app or window without the overlay on top.
pub fn hide_window(handle: WindowHandle<'_>) -> Result<(), String> {
    let (_, window) = window_from_handle(handle)?;
    window.orderOut(None);
    Ok(())
}

fn policy_name(policy: NSApplicationActivationPolicy) -> &'static str {
    match policy {
        NSApplicationActivationPolicy::Regular => "Regular",
        NSApplicationActivationPolicy::Accessory => "Accessory",
        NSApplicationActivationPolicy::Prohibited => "Prohibited",
        _ => "unknown",
    }
}

/// Performs a result's action.
pub fn perform(action: &Action) -> Result<(), String> {
    match action {
        Action::LaunchApp(path) => open_path(path),
        Action::OpenUrl(url) => open_url(url),
        Action::CopyText(text) => copy_text(text),
        Action::FocusWindow(wid) => {
            let client = windows::default_client(windows::FOCUS_TIMEOUT)?;
            windows::focus_window(&client, *wid)
        }
    }
}

/// Opens a file URL; for an `.app` bundle this launches the app, or
/// activates it when it is already running.
fn open_path(path: &Path) -> Result<(), String> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    if NSWorkspace::sharedWorkspace().openURL(&url) {
        Ok(())
    } else {
        Err(format!("NSWorkspace refused to open {}", path.display()))
    }
}

fn open_url(url: &str) -> Result<(), String> {
    let ns_url = NSURL::URLWithString(&NSString::from_str(url))
        .ok_or_else(|| format!("not a valid URL: {url}"))?;
    if NSWorkspace::sharedWorkspace().openURL(&ns_url) {
        Ok(())
    } else {
        Err(format!("NSWorkspace refused to open {url}"))
    }
}

fn copy_text(text: &str) -> Result<(), String> {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    // SAFETY: `NSPasteboardTypeString` is an immutable AppKit constant.
    let string_type = unsafe { NSPasteboardTypeString };
    if pasteboard.setString_forType(&NSString::from_str(text), string_type) {
        Ok(())
    } else {
        Err("the pasteboard refused the text".to_owned())
    }
}
