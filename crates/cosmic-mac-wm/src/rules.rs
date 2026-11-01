// SPDX-License-Identifier: GPL-3.0-or-later
//! Float-by-default classification and the "is this a window we manage?" filter.
//!
//! [`cosmic_mac_protocol::Config::should_float`] covers dialogs, non-resizable
//! windows and configured `tiling_exceptions`.  macOS adds a few window kinds
//! that are not modelled by those fields: system dialogs and floating utility
//! panels (`AXSystemDialog`, `AXFloatingWindow`) and Picture-in-Picture.  The
//! platform crate reports `is_floating`/`subrole`, but `ToplevelInfo` has no
//! `is_floating` field (see the final report), so the daemon normalises those
//! windows to `is_dialog = true` before handing them to the layout engine.
//! That makes the layout float and centre them, which is the desired
//! behaviour, at the cost of `is_dialog` being a slight over-approximation in
//! `list_windows` output.

use cosmic_mac_protocol::{Config, ToplevelInfo};

/// Built-in float rules that `Config` cannot express.
pub fn builtin_should_float(info: &ToplevelInfo) -> bool {
    let subrole = info.subrole.as_deref().unwrap_or_default();
    let role = info.role.as_deref().unwrap_or_default();
    matches!(
        subrole,
        "AXSystemDialog"
            | "AXFloatingWindow"
            | "AXDialog"
            | "AXSheet"
            | "AXPictureInPicture"
            | "AXUtilityWindow"
    ) || matches!(role, "AXSystemDialog" | "AXDialog" | "AXSheet")
        || title_is_picture_in_picture(&info.title)
}

fn title_is_picture_in_picture(title: &str) -> bool {
    let title = title.to_ascii_lowercase();
    title.contains("picture in picture") || title.contains("picture-in-picture") || title == "pip"
}

/// True when the window should float: configured rules plus macOS built-ins.
pub fn should_float(info: &ToplevelInfo, config: &Config) -> bool {
    config.should_float(info) || builtin_should_float(info)
}

/// Normalise a window for the layout engine: built-in float rules become
/// `is_dialog`, which the pure layout crate understands.
pub fn normalize_for_layout(mut info: ToplevelInfo) -> ToplevelInfo {
    if builtin_should_float(&info) {
        info.is_dialog = true;
    }
    info
}

/// Applications whose windows are never user-managed.
const SYSTEM_APP_IDS: &[&str] = &[
    "com.apple.dock",
    "com.apple.controlcenter",
    "com.apple.notificationcenterui",
    "com.apple.WindowManager",
    "com.apple.windowserver",
    "com.apple.systemuiserver",
    "com.apple.loginwindow",
    "com.apple.UserNotificationCenter",
    "com.apple.SecurityAgent",
];

/// Applications whose windows are shell surfaces (the cosmicMac panel).
///
/// The bundle identifier is the panel's identity once it ships as an app; an
/// unbundled build runs as a bare executable whose `app_id` falls back to the
/// executable name.
const SHELL_APP_IDS: &[&str] = &["cosmic-mac-panel", "com.cosmicmac.panel"];

/// Whether this window is a shell surface whose strip is reserved from the
/// work area (like a Wayland layer-shell exclusive zone) instead of being
/// managed.
///
/// Identified by application id, not by level: the panel's window is created
/// at the normal level and raised to the status level a moment later, so the
/// level may still be 0 when the window is first seen.  Callers must check
/// this before [`is_manageable`].
pub fn is_shell_window(info: &ToplevelInfo) -> bool {
    SHELL_APP_IDS
        .iter()
        .any(|app| info.app_id.eq_ignore_ascii_case(app))
}

/// Whether the daemon should manage this window at all.
///
/// Windows above the normal level (Spotlight, launcher panels, overlays),
/// minimised windows, windows in native full screen (they fill their own
/// Space), zero-sized helper windows, non-standard roles and system UI are
/// excluded.  The filter is deliberately conservative: a window
/// that is wrongly managed is much more disruptive than one that is ignored.
pub fn is_manageable(info: &ToplevelInfo) -> bool {
    !info.minimized && !info.fullscreen && is_manageable_kind(info)
}

/// [`is_manageable`] without the minimized and full-screen checks: whether
/// this is a kind of window the daemon manages once it is on the desktop.
///
/// A window that fails this (its level, application, role or size) stays
/// unmanaged while it keeps them; one that only fails [`is_manageable`] is
/// away for now and may be managed when it is back.
pub fn is_manageable_kind(info: &ToplevelInfo) -> bool {
    if info.wid == 0 {
        return false;
    }
    if info.level != 0 {
        return false;
    }
    if SYSTEM_APP_IDS
        .iter()
        .any(|app| info.app_id.eq_ignore_ascii_case(app))
    {
        return false;
    }
    if let Some(subrole) = info.subrole.as_deref()
        && matches!(subrole, "AXUnknown" | "AXSecureTextField")
    {
        return false;
    }
    if let Some(role) = info.role.as_deref()
        && !matches!(role, "AXWindow" | "AXDialog" | "AXSystemDialog" | "AXSheet")
    {
        return false;
    }
    if let Some(frame) = info.initial_frame
        && (frame.width < 40.0 || frame.height < 30.0)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic_mac_protocol::{Rect, TilingException};

    fn info() -> ToplevelInfo {
        ToplevelInfo::new(1, "com.example.app", "Document")
            .with_initial_frame(Rect::new(10.0, 20.0, 800.0, 600.0))
    }

    #[test]
    fn plain_window_tiles() {
        let config = Config::default();
        assert!(!should_float(&info(), &config));
        assert!(is_manageable(&info()));
    }

    #[test]
    fn dialog_and_non_resizable_float() {
        let config = Config::default();
        assert!(should_float(&info().with_dialog(true), &config));
        assert!(should_float(&info().with_resizable(false), &config));
    }

    #[test]
    fn configured_exception_floats() {
        let config = Config {
            tiling_exceptions: vec![TilingException::new("com.example")],
            ..Config::default()
        };
        assert!(should_float(&info(), &config));
    }

    #[test]
    fn system_dialog_and_floating_panel_float() {
        let config = Config::default();
        let mut dialog = info();
        dialog.role = Some("AXWindow".into());
        dialog.subrole = Some("AXSystemDialog".into());
        assert!(builtin_should_float(&dialog));
        assert!(should_float(&dialog, &config));

        let mut panel = info();
        panel.subrole = Some("AXFloatingWindow".into());
        assert!(should_float(&panel, &config));
        assert!(normalize_for_layout(panel).is_dialog);

        let mut utility = info();
        utility.subrole = Some("AXUtilityWindow".into());
        assert!(should_float(&utility, &config));
    }

    #[test]
    fn picture_in_picture_float_by_subrole_and_title() {
        let config = Config::default();
        let mut pip = info();
        pip.subrole = Some("AXPictureInPicture".into());
        assert!(should_float(&pip, &config));

        let mut titled = info();
        titled.title = "Picture in Picture".into();
        assert!(builtin_should_float(&titled));

        let mut hyphenated = info();
        hyphenated.title = "Video — Picture-in-Picture".into();
        assert!(builtin_should_float(&hyphenated));

        assert!(!builtin_should_float(&info()));
    }

    #[test]
    fn normalization_only_marks_builtin_floats() {
        let config = Config {
            tiling_exceptions: vec![TilingException::new("com.example")],
            ..Config::default()
        };
        // Configured exceptions are handled by the layout crate directly.
        assert!(!normalize_for_layout(info()).is_dialog);
        assert!(should_float(&info(), &config));

        let mut pip = info();
        pip.subrole = Some("AXPictureInPicture".into());
        assert!(normalize_for_layout(pip).is_dialog);
    }

    #[test]
    fn system_ui_and_tiny_windows_are_ignored() {
        let dock = ToplevelInfo::new(1, "com.apple.dock", "Dock");
        assert!(!is_manageable(&dock));

        let mut tiny = info();
        tiny.initial_frame = Some(Rect::new(0.0, 0.0, 12.0, 12.0));
        assert!(!is_manageable(&tiny));

        let mut menu = info();
        menu.role = Some("AXMenuBar".into());
        assert!(!is_manageable(&menu));

        let mut secure = info();
        secure.subrole = Some("AXSecureTextField".into());
        assert!(!is_manageable(&secure));
    }

    #[test]
    fn windows_above_the_normal_level_are_ignored() {
        assert!(!is_manageable(&info().with_level(25)));
        assert!(!is_manageable(&info().with_level(8)));
        assert!(!is_manageable(&info().with_level(-1)));
        assert!(is_manageable(&info().with_level(0)));
    }

    #[test]
    fn minimized_windows_are_ignored() {
        assert!(!is_manageable(&info().with_minimized(true)));
        assert!(is_manageable(&info().with_minimized(false)));
    }

    #[test]
    fn fullscreen_windows_are_ignored() {
        assert!(!is_manageable(&info().with_fullscreen(true)));
        assert!(is_manageable(&info().with_fullscreen(false)));
    }

    #[test]
    fn minimized_and_full_screen_windows_are_of_a_manageable_kind() {
        // Away for now, not a kind of window that is never managed.
        assert!(is_manageable_kind(&info().with_minimized(true)));
        assert!(is_manageable_kind(&info().with_fullscreen(true)));
        assert!(!is_manageable_kind(&info().with_level(25)));
        assert!(!is_manageable_kind(
            &info().with_minimized(true).with_level(25)
        ));
        let mut menu = info().with_minimized(true);
        menu.role = Some("AXMenuBar".into());
        assert!(!is_manageable_kind(&menu));
    }

    #[test]
    fn shell_windows_are_identified_by_app_id() {
        let app = |app_id: &str| ToplevelInfo::new(1, app_id, "");
        assert!(is_shell_window(&app("cosmic-mac-panel")));
        assert!(is_shell_window(&app("com.cosmicmac.panel")));
        // Case-insensitive, like the system app list.
        assert!(is_shell_window(&app("Cosmic-Mac-Panel")));
        assert!(is_shell_window(&app("COM.CosmicMac.Panel")));
        // The level does not matter: the panel is created at level 0.
        assert!(is_shell_window(&app("cosmic-mac-panel").with_level(25)));
        assert!(!is_shell_window(&info()));
        assert!(!is_shell_window(&app("")));
        assert!(!is_shell_window(&app("cosmic-mac-panel-helper")));
    }

    #[test]
    fn windows_without_frames_are_still_manageable() {
        let mut untitled = info();
        untitled.initial_frame = None;
        assert!(is_manageable(&untitled));
    }
}
