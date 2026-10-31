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
    pub visible_frame: Rect,
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

/// Enumerate active displays.
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
        let (visible_frame, scale, name) = match matched {
            Some((_, visible, scale, name)) => (visible, scale, name),
            None => (frame, 1.0, None),
        };
        displays.push(DisplayInfo {
            id,
            frame,
            visible_frame,
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
    Ok(displays)
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
