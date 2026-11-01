// SPDX-License-Identifier: GPL-3.0-or-later
//! Real macOS backend: a thin adapter over `cosmic-mac-platform`.

use std::sync::Mutex;
use std::time::Duration;

use cosmic_mac_platform::window::{self, ParkStore, WindowInfo};
use cosmic_mac_platform::{Rect as PlatformRect, display, skylight};
use cosmic_mac_protocol::{Rect, StackInfo, ToplevelInfo, WindowId};

use crate::backend::{
    AppliedFrame, DropHint, PlatformBackend, PlatformResult, SizeConstraints, WindowPresence,
};
use crate::rules;
use crate::tabbar::TabBarHandle;

/// The production backend.
pub struct MacPlatform {
    park: ParkStore,
    /// Work area of the main display: the startup value, then the last one a
    /// display change delivered.  Kept while no display is enumerated (asleep).
    work_area: Mutex<Rect>,
    /// Full frame of the main display, cached on the main thread and refreshed
    /// on every display change.  Used as the parking corner so `display_frame`
    /// does not have to call `NSScreen` from the layout thread.  Like
    /// `work_area` it keeps the last known value while no display is active,
    /// so parking during display sleep still uses the real corner.
    main_frame: Mutex<Rect>,
    tab_bar: Option<TabBarHandle>,
}

impl MacPlatform {
    /// Create the backend for the main display's `work_area` and full
    /// `main_frame`.
    ///
    /// Both must come from a real display ([`main_display_frames_now`] on the
    /// main thread); the daemon waits for one at startup instead of inventing
    /// a rect.
    pub fn new(work_area: Rect, main_frame: Rect) -> Self {
        let tab_bar = match TabBarHandle::spawn() {
            Ok(handle) => Some(handle),
            Err(error) => {
                log::warn!("stack tab bar disabled: {error}");
                None
            }
        };
        MacPlatform {
            park: ParkStore::new(),
            work_area: Mutex::new(work_area),
            main_frame: Mutex::new(main_frame),
            tab_bar,
        }
    }

    /// Update the cached work area and main display frame after a display
    /// change, so parking uses the new screen corner (a stale frame parks
    /// inside a grown screen or off a shrunk one).
    pub fn update_displays(&self, work_area: Rect, main_frame: Rect) {
        *self.work_area.lock().expect("work area poisoned") = work_area;
        *self.main_frame.lock().expect("main frame poisoned") = main_frame;
    }

    /// The cached `(work_area, main_frame)`: the last real display geometry.
    pub fn display_frames(&self) -> (Rect, Rect) {
        (
            *self.work_area.lock().expect("work area poisoned"),
            *self.main_frame.lock().expect("main frame poisoned"),
        )
    }

    fn info(&self, wid: WindowId) -> Option<ToplevelInfo> {
        window::window_info(wid as u32)
            .ok()
            .flatten()
            .map(|info| to_toplevel(&info))
    }
}

impl PlatformBackend for MacPlatform {
    fn set_frame(&self, wid: WindowId, rect: Rect) -> PlatformResult<AppliedFrame> {
        let outcome =
            window::set_frame(wid as u32, to_platform(rect)).map_err(|error| error.to_string())?;
        // `window::set_frame` already performs the size → position → size
        // workaround, one corrective retry and a read-back.  A single operation
        // only says whether this request was honoured; the direction of a
        // difference is recorded by the core and confirmed with a probe.
        Ok(AppliedFrame::new(rect, from_platform(outcome.applied)))
    }

    fn size_constraints(&self, wid: WindowId) -> PlatformResult<Option<SizeConstraints>> {
        // The platform probe shrinks and grows the window, reads back what it
        // allowed and restores the frame; results are cached per window.
        // `min`/`max` already use the final semantics: a Min axis leaves the
        // max unbounded (f64::MAX) unless a grow clamp was found, and only a
        // genuinely Frozen axis makes min == max.
        let constraints =
            window::probe_size_constraints(wid as u32).map_err(|error| error.to_string())?;
        Ok(constraints.map(|constraints| SizeConstraints {
            min: constraints.min.map(from_platform_size),
            max: constraints.max.map(from_platform_size),
        }))
    }

    fn forget_size_constraints(&self, wid: WindowId) {
        window::forget_size_constraints(wid as u32);
    }

    fn raise(&self, wid: WindowId) -> PlatformResult<()> {
        window::raise(wid as u32).map_err(|error| error.to_string())
    }

    fn focus(&self, wid: WindowId) -> PlatformResult<()> {
        window::focus_without_raise(wid as u32).map_err(|error| error.to_string())
    }

    fn close(&self, wid: WindowId) -> PlatformResult<()> {
        window::close(wid as u32).map_err(|error| error.to_string())
    }

    fn park(&self, wid: WindowId, display_frame: Rect) -> PlatformResult<Option<Rect>> {
        self.park
            .park(wid as u32, to_platform(display_frame))
            .map(|saved| saved.map(from_platform))
            .map_err(|error| error.to_string())
    }

    fn unpark(&self, wid: WindowId) -> PlatformResult<Option<Rect>> {
        self.park
            .unpark(wid as u32)
            .map(|restored| restored.then(|| self.frame(wid)).flatten())
            .map_err(|error| error.to_string())
    }

    fn saved_frame(&self, wid: WindowId) -> Option<Rect> {
        self.park.saved_frame(wid as u32).map(from_platform)
    }

    fn parked(&self) -> Vec<(WindowId, Rect)> {
        self.park
            .saved_frames()
            .into_iter()
            .map(|(wid, frame)| (u64::from(wid), from_platform(frame)))
            .collect()
    }

    fn forget(&self, wid: WindowId) {
        self.park.forget(wid as u32);
        // Drop the platform's cached probe result too: window ids are reused
        // and a stale minimum would constrain the next window with this id.
        window::forget_size_constraints(wid as u32);
    }

    fn restore_all(&self) -> Vec<(WindowId, String)> {
        self.park
            .restore_all()
            .into_iter()
            .map(|(wid, error)| (u64::from(wid), error.to_string()))
            .collect()
    }

    fn frame(&self, wid: WindowId) -> Option<Rect> {
        self.info(wid).and_then(|info| info.initial_frame)
    }

    fn window_info(&self, wid: WindowId) -> Option<ToplevelInfo> {
        self.info(wid)
    }

    fn window_presence(&self, wid: WindowId) -> WindowPresence {
        let wid = wid as u32;
        // A closed window has no owner any more (CGError 1000), while one moving to a
        // full-screen Space keeps it even though nothing else can read it.
        if skylight::window_owner_pid(wid).is_err() {
            return WindowPresence::Gone;
        }
        match skylight::window_spaces(wid) {
            Ok(spaces) if !spaces.is_empty() => WindowPresence::OnSpace {
                fullscreen: spaces.iter().any(|space| {
                    skylight::space_type(*space).ok() == Some(skylight::SPACE_TYPE_FULLSCREEN)
                }),
            },
            Ok(_) => WindowPresence::Detached,
            Err(error) => {
                log::debug!("window {wid}: cannot read its Spaces: {error}");
                WindowPresence::Detached
            }
        }
    }

    fn windows(&self) -> Vec<ToplevelInfo> {
        window::list_windows()
            .map(|windows| {
                windows
                    .iter()
                    .filter(|info| info.level == 0 && !info.is_minimized)
                    .map(to_toplevel)
                    .collect()
            })
            .unwrap_or_else(|error| {
                log::warn!("window enumeration failed: {error}");
                Vec::new()
            })
    }

    fn shell_windows(&self) -> Vec<ToplevelInfo> {
        // CG data only: shell windows are found by app id, and AX enrichment
        // would talk to every app with a window at any level.
        window::list_windows_all_levels()
            .map(|windows| {
                windows
                    .iter()
                    .map(to_toplevel)
                    .filter(rules::is_shell_window)
                    .collect()
            })
            .unwrap_or_else(|error| {
                log::warn!("shell window enumeration failed: {error}");
                Vec::new()
            })
    }

    fn set_stack_bar(&self, stack: Option<StackInfo>) -> PlatformResult<()> {
        match &self.tab_bar {
            Some(tab_bar) => tab_bar.update(stack),
            None => Ok(()),
        }
    }

    fn set_drop_hint(&self, hint: Option<DropHint>) -> PlatformResult<()> {
        match &self.tab_bar {
            Some(tab_bar) => tab_bar.drop_hint(hint),
            None => Ok(()),
        }
    }

    fn main_work_area(&self) -> Rect {
        *self.work_area.lock().expect("work area poisoned")
    }

    fn display_frame(&self, wid: WindowId) -> Rect {
        let main_frame = *self.main_frame.lock().expect("main frame poisoned");
        if let Some(frame) = self.frame(wid) {
            let (x, y) = frame.center();
            if main_frame.contains_point(x, y) {
                return main_frame;
            }
            let point = cosmic_mac_platform::Point::new(x, y);
            // Prefer the main-thread snapshot: enumerating displays here (the WM
            // thread) would lose the work areas and log a warning each time.
            let display = match display::cached_displays() {
                Some(displays) => displays.into_iter().find(|display| display.contains(point)),
                None => display::display_at(point).ok().flatten(),
            };
            if let Some(display) = display {
                return from_platform(display.frame);
            }
        }
        main_frame
    }
}

/// The main display's geometry as enumerated on the main thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayFrames {
    /// Work area (`NSScreen.visibleFrame`), or the full frame when
    /// `work_area_known` is false.
    pub work_area: Rect,
    /// Full frame, the parking corner.
    pub frame: Rect,
    /// Whether `work_area` came from `NSScreen`.  False while AppKit has not
    /// caught up with a display that CoreGraphics already lists (just woke up).
    pub work_area_known: bool,
}

impl DisplayFrames {
    /// `(work_area, frame)`.
    pub fn pair(self) -> (Rect, Rect) {
        (self.work_area, self.frame)
    }
}

/// Current main-display work area, or `None` when no display is active.
/// Call on the main thread.
pub fn main_work_area_now() -> Option<Rect> {
    main_display_frames_now().map(|frames| frames.work_area)
}

/// Current geometry of the main display (the first display if none is
/// flagged main), or `None` when no display is enumerated: a sleeping display
/// enumerates as zero displays.  Call on the main thread.
///
/// There is deliberately no fallback rect: a daemon that tiles into an
/// invented area tiles past the real screen edge and learns that edge as an
/// application size limit.
pub fn main_display_frames_now() -> Option<DisplayFrames> {
    let displays = match display::list_displays() {
        Ok(displays) => displays,
        Err(error) => {
            log::debug!("display enumeration failed: {error}");
            return None;
        }
    };
    let display = displays
        .iter()
        .find(|display| display.is_main)
        .or_else(|| displays.first())?;
    Some(DisplayFrames {
        work_area: from_platform(display.visible_frame),
        frame: from_platform(display.frame),
        work_area_known: display.visible_frame_known,
    })
}

/// How long the startup wait gives `NSScreen` to list a display CoreGraphics
/// already reports before it settles for the full frame as the work area.
pub const WORK_AREA_GRACE: Duration = Duration::from_secs(5);

/// Startup: the frames to start with, or `None` to keep waiting.
///
/// `active_for` is how long the polls have seen an active display in a row.
/// No display means wait (there is no timeout).  A display whose work area
/// `NSScreen` does not know yet is waited for up to [`WORK_AREA_GRACE`], then
/// accepted with its full frame as the work area rather than blocking a
/// running session forever.
pub fn startup_display_frames(
    enumerated: Option<DisplayFrames>,
    active_for: Duration,
) -> Option<DisplayFrames> {
    let frames = enumerated?;
    (frames.work_area_known || active_for >= WORK_AREA_GRACE).then_some(frames)
}

/// What a display-reconfiguration callback should do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DisplayUpdate {
    /// Adopt this `work_area` and main display `frame`.
    Apply {
        /// New work area.
        work_area: Rect,
        /// New main display frame.
        frame: Rect,
    },
    /// No display is enumerated (asleep or disconnected): keep the last
    /// known geometry.  The callback after wake-up delivers the real one.
    NoDisplay,
    /// The display frame is unchanged but `NSScreen` has not caught up with
    /// its work area: keep the last known work area rather than tile under
    /// the menu bar.
    WorkAreaPending,
    /// Same geometry as before (typically a wake-up): nothing to do.
    Unchanged,
}

/// Decide what a display change means, given the last known
/// `(work_area, frame)` and a fresh [`main_display_frames_now`].
pub fn next_display_frames(
    previous: (Rect, Rect),
    enumerated: Option<DisplayFrames>,
) -> DisplayUpdate {
    let Some(frames) = enumerated else {
        return DisplayUpdate::NoDisplay;
    };
    let (previous_work_area, previous_frame) = previous;
    if !frames.work_area_known && frames.frame == previous_frame {
        return DisplayUpdate::WorkAreaPending;
    }
    if frames.work_area == previous_work_area && frames.frame == previous_frame {
        return DisplayUpdate::Unchanged;
    }
    DisplayUpdate::Apply {
        work_area: frames.work_area,
        frame: frames.frame,
    }
}

/// Convert a platform window record into the protocol shape.
pub fn to_toplevel(info: &WindowInfo) -> ToplevelInfo {
    let title = info.title.clone().unwrap_or_default();
    let subrole = info.subrole.clone();
    let picture_in_picture = subrole.as_deref() == Some("AXPictureInPicture")
        || title.to_ascii_lowercase().contains("picture in picture")
        || title.to_ascii_lowercase().contains("picture-in-picture");
    let floating_subrole = matches!(
        subrole.as_deref(),
        Some("AXFloatingWindow" | "AXSystemDialog" | "AXUtilityWindow" | "AXSheet" | "AXDialog")
    );
    ToplevelInfo {
        wid: u64::from(info.wid),
        pid: info.pid,
        app_id: app_id(info),
        title,
        role: info.role.clone(),
        subrole,
        resizable: info.resizable,
        // `ToplevelInfo` has no `is_floating` field; floating panels and PiP
        // are represented as dialogs so the pure layout engine floats them.
        is_dialog: info.is_dialog || info.is_floating || floating_subrole || picture_in_picture,
        initial_frame: Some(from_platform(info.frame)),
        // Constraints are learned lazily from a probe after a clamped resize;
        // probing every window during enumeration would move them all.
        min_size: None,
        max_size: None,
        level: info.level,
        minimized: info.is_minimized,
        fullscreen: info.is_fullscreen,
    }
}

/// The bundle identifier, or the application name for an unbundled process.
///
/// A bare executable (the panel during development) has no
/// `bundleIdentifier`; its localized name, or failing that the CoreGraphics
/// owner name, is the executable name, which is what the shell-window rules
/// match on.
fn app_id(info: &WindowInfo) -> String {
    info.bundle_id
        .as_deref()
        .filter(|bundle_id| !bundle_id.is_empty())
        .or(info.app_name.as_deref())
        .unwrap_or_default()
        .to_string()
}

fn from_platform(rect: PlatformRect) -> Rect {
    Rect::new(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

fn from_platform_size(size: cosmic_mac_platform::Size) -> cosmic_mac_protocol::Size {
    cosmic_mac_protocol::Size::new(size.width, size.height)
}

fn to_platform(rect: Rect) -> PlatformRect {
    PlatformRect::new(rect.x, rect.y, rect.width, rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Rect = Rect::new(0.0, 0.0, 1728.0, 1117.0);
    const WORK_AREA: Rect = Rect::new(0.0, 25.0, 1728.0, 1092.0);

    fn known(work_area: Rect, frame: Rect) -> DisplayFrames {
        DisplayFrames {
            work_area,
            frame,
            work_area_known: true,
        }
    }

    /// A display CoreGraphics lists before `NSScreen` does.
    fn pending(frame: Rect) -> DisplayFrames {
        DisplayFrames {
            work_area: frame,
            frame,
            work_area_known: false,
        }
    }

    #[test]
    fn startup_waits_while_no_display_is_active() {
        assert_eq!(startup_display_frames(None, Duration::ZERO), None);
        // There is no timeout: an hour of display sleep still waits.
        assert_eq!(
            startup_display_frames(None, Duration::from_secs(3600)),
            None
        );
    }

    #[test]
    fn startup_takes_a_known_work_area_at_once() {
        let frames = known(WORK_AREA, FRAME);
        assert_eq!(
            startup_display_frames(Some(frames), Duration::ZERO),
            Some(frames)
        );
    }

    #[test]
    fn startup_gives_nsscreen_a_grace_period_then_takes_the_full_frame() {
        let frames = pending(FRAME);
        assert_eq!(startup_display_frames(Some(frames), Duration::ZERO), None);
        assert_eq!(
            startup_display_frames(Some(frames), WORK_AREA_GRACE - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            startup_display_frames(Some(frames), WORK_AREA_GRACE),
            Some(frames)
        );
    }

    #[test]
    fn a_display_change_without_a_display_keeps_the_last_area() {
        // The VM display fell asleep: the callback must not re-tile into an
        // invented area over the menu bar.
        assert_eq!(
            next_display_frames((WORK_AREA, FRAME), None),
            DisplayUpdate::NoDisplay
        );
    }

    #[test]
    fn waking_up_with_the_same_geometry_changes_nothing() {
        assert_eq!(
            next_display_frames((WORK_AREA, FRAME), Some(known(WORK_AREA, FRAME))),
            DisplayUpdate::Unchanged
        );
    }

    #[test]
    fn a_real_display_change_applies_the_new_area() {
        let frame = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        let work_area = Rect::new(0.0, 25.0, 1920.0, 1055.0);
        assert_eq!(
            next_display_frames((WORK_AREA, FRAME), Some(known(work_area, frame))),
            DisplayUpdate::Apply { work_area, frame }
        );
        // The Dock grew: same frame, smaller work area.
        let docked = Rect::new(0.0, 25.0, 1728.0, 1000.0);
        assert_eq!(
            next_display_frames((WORK_AREA, FRAME), Some(known(docked, FRAME))),
            DisplayUpdate::Apply {
                work_area: docked,
                frame: FRAME
            }
        );
    }

    #[test]
    fn an_unknown_work_area_keeps_the_last_one_for_the_same_frame() {
        assert_eq!(
            next_display_frames((WORK_AREA, FRAME), Some(pending(FRAME))),
            DisplayUpdate::WorkAreaPending
        );
        // A new resolution: the full frame beats a work area for the old size.
        let frame = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        assert_eq!(
            next_display_frames((WORK_AREA, FRAME), Some(pending(frame))),
            DisplayUpdate::Apply {
                work_area: frame,
                frame
            }
        );
    }

    #[test]
    fn rect_conversion_round_trips() {
        let rect = Rect::new(1.5, 2.5, 300.0, 200.0);
        assert_eq!(from_platform(to_platform(rect)), rect);
    }

    #[test]
    fn pip_and_floating_panels_become_dialogs() {
        let mut info = WindowInfo {
            wid: 1,
            pid: 2,
            bundle_id: Some("com.example.app".into()),
            app_name: None,
            title: Some("Picture in Picture".into()),
            role: Some("AXWindow".into()),
            subrole: None,
            frame: PlatformRect::new(0.0, 0.0, 320.0, 180.0),
            level: 0,
            resizable: true,
            is_dialog: false,
            is_floating: false,
            is_minimized: false,
            is_fullscreen: false,
            ax_backed: true,
        };
        assert!(to_toplevel(&info).is_dialog);

        info.title = Some("Video".into());
        info.subrole = Some("AXFloatingWindow".into());
        assert!(to_toplevel(&info).is_dialog);

        info.subrole = Some("AXPictureInPicture".into());
        assert!(to_toplevel(&info).is_dialog);

        info.subrole = Some("AXStandardWindow".into());
        assert!(!to_toplevel(&info).is_dialog);
    }

    fn platform_window() -> WindowInfo {
        WindowInfo {
            wid: 1,
            pid: 2,
            bundle_id: Some("com.example.app".into()),
            app_name: Some("Example".into()),
            title: Some("Document".into()),
            role: Some("AXWindow".into()),
            subrole: Some("AXStandardWindow".into()),
            frame: PlatformRect::new(0.0, 0.0, 800.0, 600.0),
            level: 0,
            resizable: true,
            is_dialog: false,
            is_floating: false,
            is_minimized: false,
            is_fullscreen: false,
            ax_backed: true,
        }
    }

    #[test]
    fn app_id_falls_back_to_the_app_name_without_a_bundle() {
        let mut info = platform_window();
        assert_eq!(to_toplevel(&info).app_id, "com.example.app");

        // A bare executable has no bundle identifier.
        info.bundle_id = None;
        info.app_name = Some("cosmic-mac-panel".into());
        let toplevel = to_toplevel(&info);
        assert_eq!(toplevel.app_id, "cosmic-mac-panel");
        assert!(rules::is_shell_window(&toplevel));

        info.bundle_id = Some(String::new());
        assert_eq!(to_toplevel(&info).app_id, "cosmic-mac-panel");

        info.app_name = None;
        assert_eq!(to_toplevel(&info).app_id, "");
    }

    #[test]
    fn level_and_minimized_reach_the_admission_filter() {
        let mut info = platform_window();
        let toplevel = to_toplevel(&info);
        assert_eq!((toplevel.level, toplevel.minimized), (0, false));
        assert!(rules::is_manageable(&toplevel));

        // Spotlight and launcher panels sit above the normal level.
        info.level = 25;
        let toplevel = to_toplevel(&info);
        assert_eq!(toplevel.level, 25);
        assert!(!rules::is_manageable(&toplevel));

        info.level = 0;
        info.is_minimized = true;
        let toplevel = to_toplevel(&info);
        assert!(toplevel.minimized);
        assert!(!rules::is_manageable(&toplevel));

        // A window in native full screen fills its own Space.
        info.is_minimized = false;
        info.is_fullscreen = true;
        let toplevel = to_toplevel(&info);
        assert!(toplevel.fullscreen);
        assert!(!rules::is_manageable(&toplevel));
    }
}
