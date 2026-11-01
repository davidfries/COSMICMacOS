// SPDX-License-Identifier: GPL-3.0-or-later
//! AppKit tweaks that turn the winit-created `NSWindow` into a panel.
//!
//! libcosmic/iced do not expose macOS window levels beyond winit's
//! `AlwaysOnTop` (`NSFloatingWindowLevel`), Spaces collection behaviour,
//! absolute positioning, the activation policy or non-activating windows, so
//! the panel reaches the `NSWindow` directly: iced's `window::run_with_handle`
//! hands over the raw-window-handle, whose AppKit variant carries the content
//! `NSView`; its `window` is the `NSWindow`. The callback runs on the winit
//! event-loop thread, which is the main thread on macOS, so AppKit calls are
//! allowed there.
//!
//! Activation (verified in the VM, macOS 15.6):
//! - iced builds winit's event loop with plain `EventLoop::new()`, so winit's
//!   `with_activation_policy`/`with_activate_ignoring_other_apps` are out of
//!   reach. For an unbundled binary winit's `applicationDidFinishLaunching`
//!   sets `Regular` and calls `activateIgnoringOtherApps(true)` before any
//!   window exists. Switching to `Accessory` here removes the Dock icon and
//!   hands the front back to the previously active app; the panel is
//!   frontmost for roughly 100 ms at launch.
//! - `NSWindowStyleMask::NonactivatingPanel` only takes effect when an
//!   `NSPanel` is initialised with it; setting it later (even after swapping
//!   the live window's class to an `NSPanel` subclass) still lets a click
//!   activate the app. The class swap is also unsound here: winit KVO-observes
//!   `effectiveAppearance`, so the live class is `NSKVONotifying_WinitWindow`.
//!   What `NonactivatingPanel` does underneath is AppKit's private
//!   `-[NSWindow _setPreventsActivation:]`, which works on the stock winit
//!   window: clicks reach iced, the app stays inactive, the window does not
//!   become key and the frontmost app keeps focus.

use cosmic::iced::window::raw_window_handle::{RawWindowHandle, WindowHandle};
use objc2::runtime::{Bool, NSObjectProtocol};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSScreen, NSStatusWindowLevel, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
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
    /// Activation policy of the process after configuration.
    pub activation_policy: &'static str,
    /// Whether `_setPreventsActivation:` was available and applied.
    pub prevents_activation: bool,
}

/// Turns the process into an accessory app and the window into a borderless,
/// non-activating, status-level bar on every Space, stretched across the top
/// edge of the main screen.
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

    // No Dock icon, no app menu; also returns the front to the app that was
    // active before winit activated us at launch.
    let app = NSApplication::sharedApplication(mtm);
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        log::warn!("setActivationPolicy(Accessory) was refused");
    }

    window.setStyleMask(NSWindowStyleMask::Borderless);
    let prevents_activation = set_prevents_activation(&window);
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
        activation_policy: policy_name(app.activationPolicy()),
        prevents_activation,
    })
}

/// Makes clicks on `window` not activate the app, like an `NSPanel` created
/// with `NonactivatingPanel`. Returns `false` if AppKit lacks the selector.
fn set_prevents_activation(window: &NSWindow) -> bool {
    let selector = sel!(_setPreventsActivation:);
    if !window.respondsToSelector(selector) {
        return false;
    }
    // SAFETY: private AppKit setter (used by NSPanel for NonactivatingPanel)
    // taking one BOOL and returning void; presence checked above; main
    // thread, which `&NSWindow` implies.
    let _: () = unsafe { msg_send![window, _setPreventsActivation: Bool::YES] };
    true
}

fn policy_name(policy: NSApplicationActivationPolicy) -> &'static str {
    match policy {
        NSApplicationActivationPolicy::Regular => "Regular",
        NSApplicationActivationPolicy::Accessory => "Accessory",
        NSApplicationActivationPolicy::Prohibited => "Prohibited",
        _ => "unknown",
    }
}

/// Activation policy, active flag and key window of the process, for
/// diagnostics (e.g. after a click on the panel). Main thread only.
pub fn activation_state() -> String {
    let Some(mtm) = MainThreadMarker::new() else {
        return "unknown (not on the main thread)".to_owned();
    };
    let app = NSApplication::sharedApplication(mtm);
    format!(
        "policy={} isActive={} keyWindow={:?}",
        policy_name(app.activationPolicy()),
        app.isActive(),
        app.keyWindow().map(|window| window.windowNumber()),
    )
}
