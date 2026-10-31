//! Window model, enumeration and imperative window operations.
//!
//! Enumeration uses the public `CGWindowListCopyWindowInfo` as the source of
//! truth for *what is on screen* (window IDs, stacking layer, bounds, owner
//! PID). When the Accessibility grant is present, each window is enriched with
//! AX data: title, role/subrole, resizability and dialog-ness.
//!
//! Operations that change geometry (`set_frame`, `set_position`, `set_size`,
//! `minimize`, `close`) require Accessibility. `raise` and
//! `focus_without_raise` use SkyLight and work without it.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_graphics::{
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGWindowBounds, kCGWindowIsOnscreen,
    kCGWindowLayer, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
};
use objc2_foundation::NSString;

use crate::ax::{
    ACTION_PRESS, ACTION_RAISE, ATTR_CLOSE_BUTTON, ATTR_MINIMIZED, ATTR_POSITION, ATTR_ROLE,
    ATTR_SIZE, ATTR_SUBROLE, ATTR_TITLE, ATTR_WINDOWS, AxElement,
};
use crate::error::{PlatformError, Result};
use crate::geometry::{Point, Rect, Size};
use crate::skylight;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRectMakeWithDictionaryRepresentation(
        dict: *const c_void,
        rect: *mut objc2_core_foundation::CGRect,
    ) -> bool;
}

/// A window as seen by the platform layer.
///
/// `wid` is the `CGWindowID`; it is stable for the lifetime of the window and is
/// the key used by every other primitive.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    /// Window server ID.
    pub wid: u32,
    /// Owning process ID.
    pub pid: libc::pid_t,
    /// Bundle identifier (`com.apple.TextEdit`), when resolvable.
    pub bundle_id: Option<String>,
    /// Localized application name.
    pub app_name: Option<String>,
    /// Window title. Requires Accessibility (or Screen Recording for CG titles).
    pub title: Option<String>,
    /// AX role (`AXWindow`, ...).
    pub role: Option<String>,
    /// AX subrole (`AXStandardWindow`, `AXDialog`, ...).
    pub subrole: Option<String>,
    /// Current frame in global top-left coordinates.
    pub frame: Rect,
    /// Window server level (0 = normal application window).
    pub level: i32,
    /// Whether the AX `AXSize` attribute is settable.
    pub resizable: bool,
    /// Dialog-like window (AXDialog / AXSystemDialog / AXSheet).
    pub is_dialog: bool,
    /// Floating panel (AXFloatingWindow), e.g. utility panels.
    pub is_floating: bool,
    /// Minimized (AXMinimized).
    pub is_minimized: bool,
    /// True when the record was enriched through Accessibility.
    pub ax_backed: bool,
}

impl WindowInfo {
    /// Whether this looks like a normal, user-manageable application window.
    ///
    /// Normal windows have level 0 and role `AXWindow` (when AX is available).
    /// This filter is intentionally conservative: the WM decides float rules
    /// from `is_dialog`, `resizable`, bundle id and title.
    pub fn is_normal(&self) -> bool {
        self.level == 0 && !self.is_minimized
    }
}

fn nsstring_to_string(value: Option<Retained<NSString>>) -> Option<String> {
    value.map(|value| value.to_string())
}

fn app_identity(pid: libc::pid_t) -> (Option<String>, Option<String>) {
    // SAFETY: NSRunningApplication lookup is thread-safe.
    let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        return (None, None);
    };
    (
        nsstring_to_string(app.bundleIdentifier()),
        nsstring_to_string(app.localizedName()),
    )
}

/// Read the CGWindowList record for one window.
fn cg_window_record(wid: u32, options: CGWindowListOption) -> Result<Option<WindowInfo>> {
    let Some(array) = CGWindowListCopyWindowInfo(options, wid) else {
        return Ok(None);
    };
    // SAFETY: CGWindowListCopyWindowInfo returns an array of CFDictionary.
    let array: &CFArray<CFType> = unsafe { array.cast_unchecked() };
    // SAFETY: the kCGWindow* statics are immutable CFStrings owned by
    // CoreGraphics and valid for the process lifetime.
    let key_number = unsafe { kCGWindowNumber };
    for index in 0..array.len() {
        let Some(value) = array.get(index) else {
            continue;
        };
        let Ok(dict) = value.downcast::<CFDictionary>() else {
            continue;
        };
        // SAFETY: CG window dictionaries are string-keyed.
        let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };

        let number = dict
            .get(key_number)
            .and_then(|value| value.downcast::<CFNumber>().ok())
            .and_then(|value| value.as_i64())
            .unwrap_or(0) as u32;
        if number != wid {
            continue;
        }
        return Ok(Some(cg_dict_to_window(dict)));
    }
    Ok(None)
}

fn cg_dict_to_window(dict: &CFDictionary<CFString, CFType>) -> WindowInfo {
    // SAFETY: the kCGWindow* statics are immutable CFStrings owned by
    // CoreGraphics and valid for the process lifetime.
    let (key_number, key_pid, key_layer, key_onscreen, key_name, key_owner, key_bounds) = unsafe {
        (
            kCGWindowNumber,
            kCGWindowOwnerPID,
            kCGWindowLayer,
            kCGWindowIsOnscreen,
            kCGWindowName,
            kCGWindowOwnerName,
            kCGWindowBounds,
        )
    };
    let wid = dict
        .get(key_number)
        .and_then(|value| value.downcast::<CFNumber>().ok())
        .and_then(|value| value.as_i64())
        .unwrap_or(0) as u32;
    let pid = dict
        .get(key_pid)
        .and_then(|value| value.downcast::<CFNumber>().ok())
        .and_then(|value| value.as_i32())
        .unwrap_or(0);
    let level = dict
        .get(key_layer)
        .and_then(|value| value.downcast::<CFNumber>().ok())
        .and_then(|value| value.as_i32())
        .unwrap_or(0);
    let _is_onscreen = dict
        .get(key_onscreen)
        .and_then(|value| value.downcast::<CFBoolean>().ok())
        .map(|value| value.as_bool())
        .unwrap_or(true);
    let title = dict
        .get(key_name)
        .and_then(|value| value.downcast::<CFString>().ok())
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty());
    let app_name = dict
        .get(key_owner)
        .and_then(|value| value.downcast::<CFString>().ok())
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty());

    let mut frame = Rect::default();
    if let Some(bounds) = dict.get(key_bounds)
        && let Ok(bounds) = bounds.downcast::<CFDictionary>()
    {
        let mut cg_rect = objc2_core_foundation::CGRect::default();
        // SAFETY: valid CFDictionary and out-pointer.
        let ok = unsafe {
            CGRectMakeWithDictionaryRepresentation(
                CFRetained::as_ptr(&bounds).as_ptr().cast(),
                &mut cg_rect,
            )
        };
        if ok {
            frame = Rect::from_cg(cg_rect);
        }
    }

    let (bundle_id, localized_name) = app_identity(pid);

    WindowInfo {
        wid,
        pid,
        bundle_id,
        app_name: localized_name.or(app_name),
        title,
        role: None,
        subrole: None,
        frame,
        level,
        resizable: true,
        is_dialog: false,
        is_floating: false,
        is_minimized: false,
        ax_backed: false,
    }
}

/// All on-screen windows at normal application level.
///
/// This works without Accessibility. Titles and roles will be `None` and
/// `resizable` defaults to `true` unless the process is trusted.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let options =
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let Some(array) = CGWindowListCopyWindowInfo(options, 0) else {
        return Err(PlatformError::Os {
            context: "CGWindowListCopyWindowInfo",
            message: "returned null (no GUI session?)".into(),
        });
    };
    // SAFETY: array of CFDictionary.
    let array: &CFArray<CFType> = unsafe { array.cast_unchecked() };
    let mut windows = Vec::with_capacity(array.len());
    for index in 0..array.len() {
        let Some(value) = array.get(index) else {
            continue;
        };
        let Ok(dict) = value.downcast::<CFDictionary>() else {
            continue;
        };
        // SAFETY: string-keyed dictionaries.
        let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
        let window = cg_dict_to_window(dict);
        if window.wid == 0
            || window.pid == std::process::id() as libc::pid_t
            || window.level != 0
            || !window.frame.is_non_empty()
        {
            continue;
        }
        windows.push(window);
    }

    if crate::ax::is_process_trusted() {
        enrich_with_ax(&mut windows);
    }
    Ok(windows)
}

/// AX-enrich `windows` in place. Failures are logged and ignored so that the
/// enumeration keeps working for apps that refuse AX (or after a permission
/// revocation).
fn enrich_with_ax(windows: &mut [WindowInfo]) {
    let mut by_pid: HashMap<libc::pid_t, Vec<usize>> = HashMap::new();
    for (index, window) in windows.iter().enumerate() {
        by_pid.entry(window.pid).or_default().push(index);
    }
    for (pid, indices) in by_pid {
        let Ok(app) = AxElement::application(pid) else {
            continue;
        };
        let Ok(ax_windows) = app.elements_attribute(ATTR_WINDOWS) else {
            continue;
        };
        for element in ax_windows {
            let Some(wid) = element.window_id() else {
                continue;
            };
            let Some(&index) = indices.iter().find(|&&index| windows[index].wid == wid) else {
                continue;
            };
            let window = &mut windows[index];
            window.ax_backed = true;
            window.title = element
                .string_attribute(ATTR_TITLE)
                .ok()
                .flatten()
                .filter(|title| !title.is_empty())
                .or_else(|| window.title.take());
            window.role = element.string_attribute(ATTR_ROLE).ok().flatten();
            window.subrole = element.string_attribute(ATTR_SUBROLE).ok().flatten();
            window.is_minimized = element
                .bool_attribute(ATTR_MINIMIZED)
                .ok()
                .flatten()
                .unwrap_or(false);
            window.resizable = element.is_settable(ATTR_SIZE).unwrap_or(true);
            window.is_dialog = is_dialog_role(window.role.as_deref(), window.subrole.as_deref());
            window.is_floating = window.subrole.as_deref() == Some("AXFloatingWindow");
            if let Ok(Some(frame)) = element.frame() {
                window.frame = frame;
            }
        }
    }
}

fn is_dialog_role(role: Option<&str>, subrole: Option<&str>) -> bool {
    matches!(subrole, Some("AXDialog" | "AXSystemDialog" | "AXSheet"))
        || matches!(role, Some("AXDialog" | "AXSheet"))
}

/// Enumerate one application's windows through Accessibility.
///
/// Returns [`PlatformError::AccessibilityDenied`] when the process is not
/// trusted. Off-screen and minimized windows are included (AX reports them);
/// callers that only care about visible windows should cross-check
/// [`list_windows`].
pub fn list_ax_windows(pid: libc::pid_t) -> Result<Vec<WindowInfo>> {
    let app = AxElement::application(pid)?;
    let elements = app.elements_attribute(ATTR_WINDOWS)?;
    let (bundle_id, app_name) = app_identity(pid);
    let mut windows = Vec::with_capacity(elements.len());
    for element in elements {
        let wid = element.window_id().unwrap_or(0);
        if wid == 0 {
            continue;
        }
        let frame = element.frame()?.unwrap_or_default();
        let role = element.string_attribute(ATTR_ROLE)?;
        let subrole = element.string_attribute(ATTR_SUBROLE)?;
        windows.push(WindowInfo {
            wid,
            pid,
            bundle_id: bundle_id.clone(),
            app_name: app_name.clone(),
            title: element
                .string_attribute(ATTR_TITLE)?
                .filter(|title| !title.is_empty()),
            is_dialog: is_dialog_role(role.as_deref(), subrole.as_deref()),
            is_floating: subrole.as_deref() == Some("AXFloatingWindow"),
            role,
            subrole,
            frame,
            level: skylight::window_level(wid).unwrap_or(0),
            resizable: element.is_settable(ATTR_SIZE).unwrap_or(true),
            is_minimized: element.bool_attribute(ATTR_MINIMIZED)?.unwrap_or(false),
            ax_backed: true,
        });
    }
    Ok(windows)
}

/// Refresh one window's record (including off-screen windows).
pub fn window_info(wid: u32) -> Result<Option<WindowInfo>> {
    let options =
        CGWindowListOption::OptionIncludingWindow | CGWindowListOption::ExcludeDesktopElements;
    let mut info = cg_window_record(wid, options)?;
    if let Some(info) = info.as_mut()
        && crate::ax::is_process_trusted()
        && let Ok(window) = ax_window(wid)
    {
        info.ax_backed = true;
        info.title = window.title().ok().flatten().or_else(|| info.title.take());
        info.role = window.element.string_attribute(ATTR_ROLE).ok().flatten();
        info.subrole = window.element.string_attribute(ATTR_SUBROLE).ok().flatten();
        info.resizable = window.element.is_settable(ATTR_SIZE).unwrap_or(true);
        info.is_dialog = is_dialog_role(info.role.as_deref(), info.subrole.as_deref());
        info.is_floating = info.subrole.as_deref() == Some("AXFloatingWindow");
        info.is_minimized = window
            .element
            .bool_attribute(ATTR_MINIMIZED)
            .ok()
            .flatten()
            .unwrap_or(false);
        if let Ok(Some(frame)) = window.frame() {
            info.frame = frame;
        }
    }
    Ok(info)
}

/// An AX window element plus the identifiers needed to operate on it.
#[derive(Debug, Clone)]
pub struct AxWindow {
    /// The AX window element.
    pub element: AxElement,
    /// Window server ID.
    pub wid: u32,
    /// Owning PID.
    pub pid: libc::pid_t,
}

impl AxWindow {
    /// Current frame.
    pub fn frame(&self) -> Result<Option<Rect>> {
        self.element.frame()
    }

    /// Title.
    pub fn title(&self) -> Result<Option<String>> {
        self.element.string_attribute(ATTR_TITLE)
    }

    /// Role.
    pub fn role(&self) -> Result<Option<String>> {
        self.element.string_attribute(ATTR_ROLE)
    }

    /// Subrole.
    pub fn subrole(&self) -> Result<Option<String>> {
        self.element.string_attribute(ATTR_SUBROLE)
    }

    /// Whether the window can be resized.
    pub fn resizable(&self) -> Result<bool> {
        self.element.is_settable(ATTR_SIZE)
    }

    /// Move and resize using the set-size / set-position / set-size workaround.
    ///
    /// Windows with resizing constraints (Terminal, TextEdit with a fixed wrap
    /// width, many Electron apps) refuse or clamp a size that is applied before
    /// the window has been moved to its target display. Applying the size first,
    /// then the position, then the size again converges for those windows. The
    /// final frame is read back and one more position+size pass is attempted
    /// when AppKit clamped the result.
    pub fn set_frame(&self, frame: Rect) -> Result<()> {
        self.element.set_size_attribute(ATTR_SIZE, frame.size)?;
        self.element
            .set_point_attribute(ATTR_POSITION, frame.origin)?;
        self.element.set_size_attribute(ATTR_SIZE, frame.size)?;

        // Read back. AppKit can clamp a size after the move (screen edges,
        // aspect ratio); one corrective pass is enough in practice.
        if let Ok(Some(actual)) = self.frame() {
            let off = (actual.origin.x - frame.origin.x).abs() > 1.0
                || (actual.origin.y - frame.origin.y).abs() > 1.0
                || (actual.size.width - frame.size.width).abs() > 1.0
                || (actual.size.height - frame.size.height).abs() > 1.0;
            if off {
                log::debug!(
                    "set_frame({}) requested {:?}, got {:?}; retrying",
                    self.wid,
                    frame,
                    actual
                );
                self.element
                    .set_point_attribute(ATTR_POSITION, frame.origin)?;
                self.element.set_size_attribute(ATTR_SIZE, frame.size)?;
            }
        }
        Ok(())
    }

    /// Move without resizing.
    pub fn set_position(&self, point: Point) -> Result<()> {
        self.element.set_point_attribute(ATTR_POSITION, point)
    }

    /// Resize without moving.
    pub fn set_size(&self, size: Size) -> Result<()> {
        self.element.set_size_attribute(ATTR_SIZE, size)
    }

    /// Minimize.
    pub fn minimize(&self) -> Result<()> {
        self.element.set_bool_attribute(ATTR_MINIMIZED, true)
    }

    /// Restore from the Dock.
    pub fn deminimize(&self) -> Result<()> {
        self.element.set_bool_attribute(ATTR_MINIMIZED, false)
    }

    /// Close via the AX close button.
    pub fn close(&self) -> Result<()> {
        let button = self
            .element
            .copy_attribute(ATTR_CLOSE_BUTTON)
            .map_err(|_| PlatformError::Other("window has no AXCloseButton".into()))?;
        let Some(button) =
            (unsafe { AxElement::from_borrowed(CFRetained::as_ptr(&button).as_ptr().cast()) })
        else {
            return Err(PlatformError::Other("null AXCloseButton".into()));
        };
        button.perform_action(ACTION_PRESS)
    }

    /// Raise within the application (AXRaise).
    pub fn raise_ax(&self) -> Result<()> {
        self.element.perform_action(ACTION_RAISE)
    }
}

/// Find the AX element for `wid`.
pub fn ax_window(wid: u32) -> Result<AxWindow> {
    let pid = skylight::window_owner_pid(wid)?;
    let app = AxElement::application(pid)?;
    let windows = app.elements_attribute(ATTR_WINDOWS)?;
    for element in windows {
        if element.window_id() == Some(wid) {
            return Ok(AxWindow { element, wid, pid });
        }
    }
    Err(PlatformError::WindowNotFound(wid))
}

/// Move and resize a window.
pub fn set_frame(wid: u32, frame: Rect) -> Result<()> {
    ax_window(wid)?.set_frame(frame)
}

/// Move a window without resizing.
pub fn set_position(wid: u32, point: Point) -> Result<()> {
    ax_window(wid)?.set_position(point)
}

/// Resize a window without moving it.
pub fn set_size(wid: u32, size: Size) -> Result<()> {
    ax_window(wid)?.set_size(size)
}

/// Raise a window.
///
/// Uses a SkyLight order-above transaction (works without Accessibility) and,
/// when AX is available, `AXRaise` so the owning app also treats it as front.
pub fn raise(wid: u32) -> Result<()> {
    let trusted = crate::ax::is_process_trusted();
    let mut ax_raised = false;
    if trusted && let Ok(window) = ax_window(wid) {
        ax_raised = window.raise_ax().is_ok();
    }

    let cid = skylight::main_connection()?;
    // SAFETY: valid connection and window id.
    let ordered = unsafe { skylight::SLSOrderWindow(cid, wid, skylight::ORDER_ABOVE, 0)? };
    if ax_raised || ordered == 0 {
        return Ok(());
    }
    // Ordering another process's window through SkyLight is rejected unless the
    // caller holds Accessibility. Without it the AXRaise path is unavailable
    // too, so report the permission instead of an opaque CGError.
    if !trusted {
        log::debug!("SLSOrderWindow({wid}) failed with {ordered}; no Accessibility grant");
        return Err(PlatformError::AccessibilityDenied);
    }
    Err(PlatformError::SkyLight {
        call: "SLSOrderWindow",
        code: ordered,
    })
}

/// Focus a window without raising it or reordering other windows.
///
/// This is the `_SLPSSetFrontProcessWithOptions` + `SLPSPostEventRecordTo`
/// pattern used by yabai, rift and glide. It makes the owning app frontmost and
/// the window key, but the synthetic event records do not change the z order of
/// the app's other windows. Credits: Hammerspoon issue #370, rift
/// `make_key_window`.
pub fn focus_without_raise(wid: u32) -> Result<()> {
    let pid = skylight::window_owner_pid(wid)?;
    let psn = skylight::process_serial_number(pid)?;

    // Event record layout (0x100 bytes) used by the WindowServer's CPS path.
    let mut event1 = [0u8; 0x100];
    event1[0x04] = 0xf8;
    event1[0x08] = 0x01;
    event1[0x3a] = 0x10;
    event1[0x3c..0x40].copy_from_slice(&wid.to_le_bytes());
    event1[0x20..0x30].fill(0xff);
    let mut event2 = event1;
    event2[0x08] = 0x02;

    // `kCPSUserGenerated`
    const K_CPS_USER_GENERATED: u32 = 0x200;

    log::debug!(
        "focus_without_raise: wid={wid} pid={pid} psn=({:#x},{:#x})",
        psn.high,
        psn.low
    );

    // SAFETY: PSN is valid for the live process; the event records are stack
    // buffers of the documented size.
    unsafe {
        let front = skylight::_SLPSSetFrontProcessWithOptions(&psn, wid, K_CPS_USER_GENERATED)?;
        log::debug!("_SLPSSetFrontProcessWithOptions -> {front}");
        crate::error::cg_ok("_SLPSSetFrontProcessWithOptions", front)?;
        let first = skylight::SLPSPostEventRecordTo(&psn, event1.as_ptr())?;
        log::debug!("SLPSPostEventRecordTo(1) -> {first}");
        if first != 0 && !crate::ax::is_process_trusted() {
            // kCGErrorInvalidConnection here means the process is not allowed to
            // post CPS event records to another application.
            return Err(PlatformError::AccessibilityDenied);
        }
        crate::error::cg_ok("SLPSPostEventRecordTo", first)?;
        let second = skylight::SLPSPostEventRecordTo(&psn, event2.as_ptr())?;
        log::debug!("SLPSPostEventRecordTo(2) -> {second}");
        crate::error::cg_ok("SLPSPostEventRecordTo", second)?;
    }
    Ok(())
}

/// Bring an application to the front (no-op for the already-front app).
pub fn activate_app(pid: libc::pid_t) -> Result<()> {
    let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        return Err(PlatformError::Other(format!(
            "no running application for pid {pid}"
        )));
    };
    app.activateWithOptions(NSApplicationActivationOptions(0));
    Ok(())
}

/// Minimize a window.
pub fn minimize(wid: u32) -> Result<()> {
    ax_window(wid)?.minimize()
}

/// Close a window through its AX close button.
pub fn close(wid: u32) -> Result<()> {
    ax_window(wid)?.close()
}

/// Saved frames for parked windows.
///
/// The WM parks inactive workspace members and inactive stack members at the
/// bottom-right corner of their display with 1 px visible. This store keeps the
/// original frame so `unpark` can restore it exactly. It is in-memory; the
/// daemon's crash journal is a separate concern.
#[derive(Debug, Default)]
pub struct ParkStore {
    saved: Mutex<HashMap<u32, Rect>>,
}

impl ParkStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Park `wid` at the bottom-right corner of `display_frame`.
    ///
    /// The first call remembers the window's current frame. Subsequent calls
    /// (for an already parked window) do not overwrite the saved frame.
    /// Returns the frame that was saved.
    pub fn park(&self, wid: u32, display_frame: Rect) -> Result<Option<Rect>> {
        let current = ax_window(wid)?.frame()?.ok_or_else(|| {
            PlatformError::Other(format!("window {wid} has no AXPosition/AXSize"))
        })?;
        let saved = {
            let mut saved = self.saved.lock().expect("park store poisoned");
            *saved.entry(wid).or_insert(current)
        };
        // 1 px of the window's left edge remains visible at the display corner.
        let target = Point::new(display_frame.max_x() - 1.0, display_frame.max_y() - 1.0);
        set_position(wid, target)?;
        Ok(Some(saved))
    }

    /// Restore a parked window. Returns `true` when a saved frame existed.
    pub fn unpark(&self, wid: u32) -> Result<bool> {
        let saved = {
            let mut saved = self.saved.lock().expect("park store poisoned");
            saved.remove(&wid)
        };
        if let Some(frame) = saved {
            set_frame(wid, frame)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// The saved frame for `wid`, if parked.
    pub fn saved_frame(&self, wid: u32) -> Option<Rect> {
        self.saved
            .lock()
            .expect("park store poisoned")
            .get(&wid)
            .copied()
    }

    /// Forget a window without touching it (e.g. it was closed).
    pub fn forget(&self, wid: u32) -> Option<Rect> {
        self.saved.lock().expect("park store poisoned").remove(&wid)
    }

    /// All parked windows.
    pub fn saved_frames(&self) -> Vec<(u32, Rect)> {
        self.saved
            .lock()
            .expect("park store poisoned")
            .iter()
            .map(|(&wid, &frame)| (wid, frame))
            .collect()
    }

    /// Number of parked windows.
    pub fn len(&self) -> usize {
        self.saved.lock().expect("park store poisoned").len()
    }

    /// Whether nothing is parked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Restore every parked window. Errors are collected per window so one
    /// closed window does not prevent the others from being restored.
    pub fn restore_all(&self) -> Vec<(u32, PlatformError)> {
        let entries: Vec<_> = self
            .saved
            .lock()
            .expect("park store poisoned")
            .drain()
            .collect();
        let mut errors = Vec::new();
        for (wid, frame) in entries {
            if let Err(err) = set_frame(wid, frame) {
                errors.push((wid, err));
            }
        }
        errors
    }
}
