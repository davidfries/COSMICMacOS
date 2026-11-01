//! Display enumeration and display-change notifications.
//!
//! Display IDs and frames come from CoreGraphics (thread-safe). Work areas
//! (`visibleFrame`, i.e. the frame minus the menu bar and Dock) come from
//! `NSScreen` and therefore need to be read on the main thread. When
//! [`list_displays`] is called off the main thread it falls back to the full
//! display frame and logs a warning; the WM daemon enumerates displays on its
//! main thread.

use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayChangeSummaryFlags,
    CGDisplayRegisterReconfigurationCallback, CGDisplayRemoveReconfigurationCallback,
    CGGetActiveDisplayList, CGMainDisplayID,
};

use crate::coords;
use crate::error::{PlatformError, Result};
use crate::geometry::Rect;

/// A display as seen by the platform layer.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    /// `CGDirectDisplayID`.
    pub id: CGDirectDisplayID,
    /// Full frame in global top-left coordinates.
    pub frame: Rect,
    /// Work area (menu bar and Dock excluded) in global top-left coordinates.
    ///
    /// The full [`frame`](Self::frame) when no `NSScreen` matched this display;
    /// see [`visible_frame_known`](Self::visible_frame_known).
    pub visible_frame: Rect,
    /// Whether [`visible_frame`](Self::visible_frame) came from
    /// `NSScreen.visibleFrame`.  False off the main thread, and on the main
    /// thread while `NSScreen` has not caught up with a display change (a
    /// display that just woke up can be active in CoreGraphics before AppKit
    /// lists it).
    pub visible_frame_known: bool,
    /// Backing scale factor (2.0 on Retina displays).
    pub scale: f64,
    /// Whether this is the display with the menu bar.
    pub is_main: bool,
    /// Localized display name (`NSScreen.localizedName`, macOS 10.15+).
    pub name: Option<String>,
}

impl DisplayInfo {
    /// Whether the display contains `point` (top-left global coordinates).
    pub fn contains(&self, point: crate::Point) -> bool {
        self.frame.contains(point)
    }
}

/// Height of the main display, the anchor for AppKit's bottom-left origin.
pub fn main_display_height() -> f64 {
    let main = CGMainDisplayID();
    CGDisplayBounds(main).size.height
}

/// The CG display ID of the main display.
pub fn main_display_id() -> CGDirectDisplayID {
    CGMainDisplayID()
}

/// Last enumeration produced on the main thread, where `NSScreen` work areas are
/// available.
static SNAPSHOT: OnceLock<Mutex<Vec<DisplayInfo>>> = OnceLock::new();

/// Displays as of the last [`list_displays`] call made **on the main thread**
/// that found at least one display, or `None` if that never happened.
///
/// Off-main-thread callers (the WM thread's probes and `display_frame`) use this
/// instead of [`list_displays`], which would log a warning and fall back to full
/// display frames.  The daemon populates it at startup on the main thread and
/// refreshes it from the display-change handler, which re-enumerates the main
/// display on the main thread (`macos::main_display_frames_now`).
///
/// An enumeration that finds no display (a sleeping display enumerates as
/// zero displays) does not clear the snapshot: probes and parking keep the
/// last known geometry instead of losing their display while it sleeps.
pub fn cached_displays() -> Option<Vec<DisplayInfo>> {
    let displays = SNAPSHOT.get()?.lock().ok()?;
    (!displays.is_empty()).then(|| displays.clone())
}

/// The main display from the snapshot; see [`cached_displays`].
pub fn cached_main_display() -> Option<DisplayInfo> {
    cached_displays()?
        .into_iter()
        .find(|display| display.is_main)
}

/// Number of active displays, without enumerating `NSScreen`.
///
/// Zero while the only display sleeps.  Cheap enough to poll.
pub fn active_display_count() -> Result<u32> {
    let mut count: u32 = 0;
    // SAFETY: a null list with a zero capacity asks only for the count.
    let err = unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) };
    if err.0 != 0 {
        return Err(PlatformError::Os {
            context: "CGGetActiveDisplayList",
            message: format!("CGError {}", err.0),
        });
    }
    Ok(count)
}

/// Enumerate active displays.
///
/// Calls made on the main thread also refresh the snapshot behind
/// [`cached_displays`] (an empty result keeps the previous snapshot).
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    let mut ids = [0u32; 32];
    let mut count: u32 = 0;
    // SAFETY: fixed-size output buffer and count pointer.
    let err = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if err.0 != 0 {
        return Err(PlatformError::Os {
            context: "CGGetActiveDisplayList",
            message: format!("CGError {}", err.0),
        });
    }
    let main_id = CGMainDisplayID();
    let anchor = CGDisplayBounds(main_id).size.height;

    // NSScreen is only valid on the main thread; collect the work areas there.
    let on_main_thread = MainThreadMarker::new().is_some();
    let screens: Vec<(Rect, Rect, f64, Option<String>)> = if let Some(mtm) = MainThreadMarker::new()
    {
        NSScreen::screens(mtm)
            .iter()
            .map(|screen| {
                let frame = coords::ns_to_ax(Rect::from_cg(screen.frame()), anchor);
                let visible = coords::ns_to_ax(Rect::from_cg(screen.visibleFrame()), anchor);
                let scale = screen.backingScaleFactor();
                let name = Some(screen.localizedName().to_string());
                (frame, visible, scale, name)
            })
            .collect()
    } else {
        log::warn!(
            "list_displays() called off the main thread; visibleFrame unavailable, using full \
             display frames as work areas"
        );
        Vec::new()
    };

    let mut displays = Vec::with_capacity(count as usize);
    for &id in &ids[..count as usize] {
        let frame = Rect::from_cg(CGDisplayBounds(id));
        let matched = screens
            .iter()
            .find(|(screen_frame, _, _, _)| {
                (screen_frame.origin.x - frame.origin.x).abs() < 1.0
                    && (screen_frame.origin.y - frame.origin.y).abs() < 1.0
                    && (screen_frame.size.width - frame.size.width).abs() < 1.0
                    && (screen_frame.size.height - frame.size.height).abs() < 1.0
            })
            .cloned();
        let (visible_frame, visible_frame_known, scale, name) = match matched {
            Some((_, visible, scale, name)) => (visible, true, scale, name),
            None => (frame, false, 1.0, None),
        };
        displays.push(DisplayInfo {
            id,
            frame,
            visible_frame,
            visible_frame_known,
            scale,
            is_main: id == main_id,
            name,
        });
    }
    displays.sort_by(|a, b| {
        b.is_main
            .cmp(&a.is_main)
            .then_with(|| a.frame.origin.x.total_cmp(&b.frame.origin.x))
    });
    if on_main_thread {
        let slot = SNAPSHOT.get_or_init(|| Mutex::new(Vec::new()));
        if let Ok(mut snapshot) = slot.lock() {
            refresh_snapshot(&mut snapshot, &displays);
        }
    }
    Ok(displays)
}

/// Replace the snapshot with a main-thread enumeration, unless it found no
/// display: the last known geometry stays in place while a display sleeps.
fn refresh_snapshot(snapshot: &mut Vec<DisplayInfo>, displays: &[DisplayInfo]) {
    if !displays.is_empty() {
        *snapshot = displays.to_vec();
    }
}

/// The main display.
pub fn main_display() -> Result<DisplayInfo> {
    let displays = list_displays()?;
    displays
        .into_iter()
        .find(|display| display.is_main)
        .ok_or_else(|| PlatformError::Other("no main display".into()))
}

/// The display whose frame contains `point` (top-left global).
pub fn display_at(point: crate::Point) -> Result<Option<DisplayInfo>> {
    Ok(list_displays()?
        .into_iter()
        .find(|display| display.contains(point)))
}

/// Whether "Displays have separate Spaces" is enabled.
///
/// cosmicMac requires it to be **off** so that corner parking cannot leak
/// windows onto another display's Space. Returns `None` when called off the
/// main thread (AppKit refuses to answer).
pub fn screens_have_separate_spaces() -> Option<bool> {
    let mtm = MainThreadMarker::new()?;
    Some(NSScreen::screensHaveSeparateSpaces(mtm))
}

/// Callback for display configuration changes.
pub type DisplayChangeHandler = std::sync::Arc<dyn Fn() + Send + Sync>;

static DISPLAY_HANDLER: OnceLock<Mutex<Option<DisplayChangeHandler>>> = OnceLock::new();

unsafe extern "C-unwind" fn display_changed(
    _display: CGDirectDisplayID,
    _flags: CGDisplayChangeSummaryFlags,
    _user_info: *mut c_void,
) {
    let handler = DISPLAY_HANDLER
        .get()
        .and_then(|slot| slot.lock().ok())
        .and_then(|slot| slot.clone());
    if let Some(handler) = handler {
        handler();
    }
}

/// Register a display-configuration callback.
///
/// The callback runs on the run loop of the thread that registered it (usually
/// the main thread). Calling this more than once replaces the handler.
pub fn set_display_change_handler(handler: DisplayChangeHandler) -> Result<()> {
    let slot = DISPLAY_HANDLER.get_or_init(|| Mutex::new(None));
    let first = slot.lock().expect("display handler poisoned").is_none();
    *slot.lock().expect("display handler poisoned") = Some(handler);
    if first {
        // SAFETY: callback is a valid C function and user_info is null.
        let err = unsafe {
            CGDisplayRegisterReconfigurationCallback(Some(display_changed), std::ptr::null_mut())
        };
        if err.0 != 0 {
            return Err(PlatformError::Os {
                context: "CGDisplayRegisterReconfigurationCallback",
                message: format!("CGError {}", err.0),
            });
        }
    }
    Ok(())
}

/// Remove the display-configuration callback.
pub fn clear_display_change_handler() {
    if let Some(slot) = DISPLAY_HANDLER.get() {
        *slot.lock().expect("display handler poisoned") = None;
    }
    // SAFETY: unregistering the same callback is safe even if it was never
    // registered; CoreGraphics returns an error which we ignore here.
    unsafe {
        let _ = CGDisplayRemoveReconfigurationCallback(Some(display_changed), std::ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test threads are never the process main thread, so enumerating from here
    /// must not populate the snapshot (it would hold full frames, not work areas).
    #[test]
    fn enumerating_off_the_main_thread_does_not_populate_the_snapshot() {
        assert!(MainThreadMarker::new().is_none());
        assert!(cached_displays().is_none());
        assert!(cached_main_display().is_none());
        let _ = list_displays();
        assert!(cached_displays().is_none());
        assert!(cached_main_display().is_none());
    }

    #[test]
    fn the_display_count_matches_the_enumeration() {
        // Both are zero on a headless host and while the display sleeps.
        if let (Ok(count), Ok(displays)) = (active_display_count(), list_displays()) {
            assert_eq!(count as usize, displays.len());
        }
    }

    fn display(id: CGDirectDisplayID, width: f64) -> DisplayInfo {
        DisplayInfo {
            id,
            frame: Rect::new(0.0, 0.0, width, 1117.0),
            visible_frame: Rect::new(0.0, 25.0, width, 1092.0),
            visible_frame_known: true,
            scale: 2.0,
            is_main: true,
            name: Some("Display".into()),
        }
    }

    #[test]
    fn an_empty_enumeration_keeps_the_last_known_snapshot() {
        let mut snapshot = Vec::new();
        refresh_snapshot(&mut snapshot, &[display(1, 1728.0)]);
        assert_eq!(snapshot, vec![display(1, 1728.0)]);

        // The display went to sleep: CoreGraphics lists nothing.
        refresh_snapshot(&mut snapshot, &[]);
        assert_eq!(snapshot, vec![display(1, 1728.0)]);

        // It woke up with a new resolution.
        refresh_snapshot(&mut snapshot, &[display(1, 1920.0)]);
        assert_eq!(snapshot, vec![display(1, 1920.0)]);
    }
}
