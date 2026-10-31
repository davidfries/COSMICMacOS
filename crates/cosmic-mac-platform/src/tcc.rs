//! TCC / permission presence checks.
//!
//! cosmicMac needs two one-time GUI grants in the VM:
//!
//! * **Accessibility** for the WM daemon (AX window control, event tap).
//! * **Screen Recording** for `screencapture` over SSH (screenshots) and for
//!   milestone 3's ScreenCaptureKit thumbnails.
//!
//! TCC keys the grant on the code signature's designated requirement. Ad-hoc
//! signatures change on every rebuild and lose the grant; `scripts/sign.sh`
//! signs with a stable self-signed identity so the grant survives redeploys.

use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

use crate::ax;
use crate::error::{PlatformError, Result};

/// Whether the process currently has the Accessibility grant.
pub fn accessibility_trusted() -> bool {
    ax::is_process_trusted()
}

/// Check Accessibility and optionally ask macOS to show the grant prompt.
pub fn request_accessibility(prompt: bool) -> bool {
    ax::is_process_trusted_with_prompt(prompt)
}

/// Return an error when Accessibility is missing.
pub fn require_accessibility() -> Result<()> {
    if accessibility_trusted() {
        Ok(())
    } else {
        Err(PlatformError::AccessibilityDenied)
    }
}

/// Whether the process (or its parent, for `screencapture` over SSH) has the
/// Screen Recording grant.
pub fn screen_recording_trusted() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Ask for Screen Recording. Returns the current state; the grant only takes
/// effect after a relaunch of the process.
pub fn request_screen_recording() -> bool {
    CGRequestScreenCaptureAccess()
}

/// A snapshot of the permissions cosmicMac cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionStatus {
    /// Accessibility (AX) grant for this binary.
    pub accessibility: bool,
    /// Screen Recording grant for this binary.
    pub screen_recording: bool,
    /// Whether SkyLight could be loaded and a WindowServer connection opened.
    pub skylight: bool,
    /// `NSScreen.screensHaveSeparateSpaces` when called on the main thread.
    pub separate_spaces: Option<bool>,
}

impl PermissionStatus {
    /// Human-readable startup report.
    pub fn report(&self) -> String {
        let mut report = String::new();
        report.push_str(if self.accessibility {
            "Accessibility:      granted\n"
        } else {
            "Accessibility:      NOT GRANTED (window control and the event tap will fail)\n"
        });
        report.push_str(if self.screen_recording {
            "Screen Recording:   granted\n"
        } else {
            "Screen Recording:   NOT GRANTED (screencapture over SSH returns a black image)\n"
        });
        report.push_str(if self.skylight {
            "SkyLight:           connected\n"
        } else {
            "SkyLight:           UNAVAILABLE (no GUI session or private framework moved)\n"
        });
        match self.separate_spaces {
            Some(true) => report.push_str(
                "Displays have separate Spaces: ON  <-- turn this OFF for corner parking\n",
            ),
            Some(false) => report.push_str("Displays have separate Spaces: off\n"),
            None => report
                .push_str("Displays have separate Spaces: unknown (call on the main thread)\n"),
        }
        report
    }
}

/// Collect the current permission state.
pub fn status() -> PermissionStatus {
    PermissionStatus {
        accessibility: accessibility_trusted(),
        screen_recording: screen_recording_trusted(),
        skylight: crate::skylight::main_connection().is_ok(),
        separate_spaces: crate::display::screens_have_separate_spaces(),
    }
}
