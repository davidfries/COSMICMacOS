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
//! - The launcher runs from `cosmic-mac-launcher.app` (`Info.plist` with a
//!   bundle identifier and `LSUIElement`, assembled by
//!   `scripts/bundle.sh`), so winit sees a bundle identifier and leaves the
//!   activation policy to `LSUIElement` (Accessory from the start, no Dock
//!   icon) instead of forcing `Regular` at launch. The plain binary embeds
//!   the same `Info.plist` (`build.rs`), which macOS does not honour for
//!   unbundled executables; for it [`show_window`] switches the policy to
//!   `Accessory` (a Dock icon may flash at launch).
//! - [`show_window`] activates the app with `activateIgnoringOtherApps:`
//!   (the launcher is told to show by a background process, so cooperative
//!   `activate` may be declined) and makes the window key. winit's window
//!   class overrides `canBecomeKeyWindow`, so a borderless window can be
//!   key.
//! - [`hide_window`] orders the window out and hides the app, so AppKit
//!   gives focus back to the app that had it. winit activates every app at
//!   launch, so a hidden start calls [`yield_activation`] for the same.
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
use crate::plugins::{terminal, windows};

/// The launcher `NSWindow` after [`show_window`], for the log.
#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub window_number: isize,
    /// Frame in AppKit screen coordinates (bottom-left origin).
    pub frame: Frame,
    /// The main screen's `visibleFrame` the window was centred in.
    pub visible_frame: Frame,
    pub level: isize,
    /// Activation policy before and after showing.
    pub initial_policy: &'static str,
    pub policy: &'static str,
    /// Bundle identifier AppKit sees for the process; `None` means the
    /// launcher runs unbundled (winit then forces `Regular` at launch).
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

/// Borderless, shadowless, modal-panel level, on every Space. Idempotent.
fn prepare_window(window: &NSWindow) {
    window.setStyleMask(NSWindowStyleMask::Borderless);
    window.setHasShadow(false);
    window.setLevel(NSModalPanelWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
}

/// Prepares the launcher window ([`prepare_window`]), centres it on the
/// main screen, shows it and gives it keyboard focus. Signature matches
/// iced's `window::run_with_handle` callback.
pub fn show_window(handle: WindowHandle<'_>) -> Result<WindowInfo, String> {
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

    prepare_window(&window);
    // `hide_window` hid the app; a hidden app's windows stay off screen.
    if app.isHidden() {
        app.unhideWithoutActivation();
    }

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

/// Orders the window out right away (the caller closes it afterwards) and,
/// while the launcher is the active app, hides the app so AppKit activates
/// the app that was active before (an app launched or a window focused by
/// the activated result takes over when it comes up).
pub fn hide_window(handle: WindowHandle<'_>) -> Result<(), String> {
    let (mtm, window) = window_from_handle(handle)?;
    window.orderOut(None);
    let app = NSApplication::sharedApplication(mtm);
    if app.isActive() {
        app.hide(None);
    }
    Ok(())
}

/// Gives up activation while no launcher window is shown: winit activates
/// the app when it finishes launching, which would take focus from the
/// user's app at a hidden (launchd) start. Returns whether the app was
/// active. Must run on the main thread (iced's `update` does).
pub fn yield_activation() -> Result<bool, String> {
    let mtm = MainThreadMarker::new().ok_or_else(|| "not on the main thread".to_owned())?;
    let app = NSApplication::sharedApplication(mtm);
    let active = app.isActive();
    if active {
        app.hide(None);
    }
    Ok(active)
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
        Action::LaunchApp(path) | Action::OpenFile(path) => open_path(path),
        Action::RunInTerminal(command) => {
            let script = terminal::terminal_script(command, &user_shell());
            let path = terminal::write_script(&std::env::temp_dir(), &script)
                .map_err(|error| format!("cannot write the terminal script: {error}"))?;
            open_path(&path)
        }
        Action::RunInBackground(command) => terminal::spawn_background(command, &user_shell())
            .map_err(|error| format!("cannot start {command:?}: {error}")),
        Action::OpenUrl(url) => open_url(url),
        Action::CopyText(text) => copy_text(text),
        Action::FocusWindow(wid) => {
            let client = windows::default_client(windows::FOCUS_TIMEOUT)?;
            windows::focus_window(&client, *wid)
        }
    }
}

fn user_shell() -> String {
    terminal::user_shell(std::env::var("SHELL").ok().as_deref())
}

/// Opens a file URL with its default app; for an `.app` bundle this
/// launches the app, or activates it when it is already running, and a
/// `.command` file runs in a new Terminal window.
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
