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
use std::sync::{Mutex, OnceLock};

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
    ACTION_PRESS, ACTION_RAISE, ATTR_CLOSE_BUTTON, ATTR_MAX_SIZE, ATTR_MAXIMUM_SIZE,
    ATTR_MINIMIZED, ATTR_POSITION, ATTR_ROLE, ATTR_SIZE, ATTR_SUBROLE, ATTR_TITLE, ATTR_WINDOWS,
    AxElement,
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

/// How an application responded to one requested dimension of a geometry change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DimensionOutcome {
    /// The application applied the requested value (within [`FRAME_TOLERANCE`]).
    Applied,
    /// The application applied a different value: it has a minimum or maximum on this axis.
    Clamped,
    /// The application ignored the change entirely; the dimension is frozen.
    Frozen,
}

/// Result of a geometry operation: what was asked for and what the window actually became.
///
/// macOS applications silently clamp or ignore `AXSize` / `AXPosition` changes.  Returning the
/// read-back frame lets the daemon learn the real constraint instead of assuming the requested
/// rectangle was applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppliedFrame {
    /// The frame that was requested.
    pub requested: Rect,
    /// The frame read back after the operation (and one corrective retry, for `set_frame`).
    pub applied: Rect,
    /// Per-axis outcome for the width.
    pub width: DimensionOutcome,
    /// Per-axis outcome for the height.
    pub height: DimensionOutcome,
    /// Outcome for the top-left origin (all components together).
    pub position: DimensionOutcome,
    /// True when any part of the applied frame differs from the request.
    pub clamped: bool,
}

impl AppliedFrame {
    /// Classify a read-back frame against the request and the frame before the operation.
    fn from_frames(requested: Rect, applied: Rect, previous: Option<Rect>) -> Self {
        let width = classify_dimension(
            requested.size.width,
            applied.size.width,
            previous.map(|frame| frame.size.width),
        );
        let height = classify_dimension(
            requested.size.height,
            applied.size.height,
            previous.map(|frame| frame.size.height),
        );
        let position = classify_position(
            requested.origin,
            applied.origin,
            previous.map(|frame| frame.origin),
        );
        AppliedFrame {
            requested,
            applied,
            width,
            height,
            position,
            clamped: width != DimensionOutcome::Applied
                || height != DimensionOutcome::Applied
                || position != DimensionOutcome::Applied,
        }
    }

    /// True when the window ended up exactly where it was asked to go.
    pub fn matches_requested(&self) -> bool {
        !self.clamped
    }

    /// True when the width or height differs from the request.
    pub fn size_clamped(&self) -> bool {
        self.width != DimensionOutcome::Applied || self.height != DimensionOutcome::Applied
    }

    /// True when the origin differs from the request.
    pub fn position_clamped(&self) -> bool {
        self.position != DimensionOutcome::Applied
    }
}

/// Per-axis constraint discovered by [`AxWindow::probe_size_constraints`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AxisConstraint {
    /// The axis followed the request; no minimum was detected.
    #[default]
    Free,
    /// The application refused to change this dimension at all.
    Frozen {
        /// The value the dimension is stuck at.
        value: f64,
    },
    /// The application refused to shrink below this value.
    Min {
        /// Smallest accepted value in logical pixels.
        min: f64,
    },
}

impl AxisConstraint {
    /// Smallest accepted value on this axis (`0.0` when free).
    pub fn minimum(self) -> f64 {
        match self {
            AxisConstraint::Free => 0.0,
            AxisConstraint::Frozen { value } => value,
            AxisConstraint::Min { min } => min,
        }
    }

    /// Largest accepted value on this axis, if the axis is frozen.
    pub fn maximum(self) -> Option<f64> {
        match self {
            AxisConstraint::Frozen { value } => Some(value),
            AxisConstraint::Free | AxisConstraint::Min { .. } => None,
        }
    }

    /// True when no constraint was detected.
    pub fn is_free(self) -> bool {
        matches!(self, AxisConstraint::Free)
    }
}

/// Minimum and maximum size an application accepts.
///
/// The probe detects the minimum by momentarily shrinking the window; an application that
/// exposes `AXMaxSize` / `AXMaximumSize` also reports its maximum without a grow probe.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SizeConstraints {
    /// Minimum accepted size.  `None` when no axis has a minimum.
    pub min: Option<Size>,
    /// Maximum accepted size.  `None` when no axis has a maximum.  A free axis is
    /// `f64::MAX` ("unbounded").
    pub max: Option<Size>,
    /// Width constraint as detected by the probe.
    pub width: AxisConstraint,
    /// Height constraint as detected by the probe.
    pub height: AxisConstraint,
}

impl SizeConstraints {
    /// Build a result for a window whose probe target equalled its current size.
    fn unconstrained(max: Option<Size>) -> Self {
        SizeConstraints {
            min: None,
            max,
            width: AxisConstraint::Free,
            height: AxisConstraint::Free,
        }
    }

    /// Pure logic behind the probe: classify the applied size and merge a declared maximum.
    ///
    /// `original` is the size before probing, `probe` the requested tiny size and `applied`
    /// the size the application actually produced.
    fn from_probe(original: Size, probe: Size, applied: Size, declared_max: Option<Size>) -> Self {
        let width = classify_axis(original.width, probe.width, applied.width);
        let height = classify_axis(original.height, probe.height, applied.height);
        let min = if width.is_free() && height.is_free() {
            None
        } else {
            Some(Size::new(width.minimum(), height.minimum()))
        };
        let max = merge_max(width, height, declared_max);
        SizeConstraints {
            min,
            max,
            width,
            height,
        }
    }
}

/// Tolerance used when comparing requested and applied geometry, in logical pixels.
pub const FRAME_TOLERANCE: f64 = 1.0;

/// Size the probe shrinks to before reading back the application's minimum.
const PROBE_SIZE: f64 = 1.0;

fn approx_equal(a: f64, b: f64) -> bool {
    (a - b).abs() <= FRAME_TOLERANCE
}

fn frame_differs(a: Rect, b: Rect) -> bool {
    !approx_equal(a.origin.x, b.origin.x)
        || !approx_equal(a.origin.y, b.origin.y)
        || !approx_equal(a.size.width, b.size.width)
        || !approx_equal(a.size.height, b.size.height)
}

/// Classify one requested dimension against what the application applied and what it was
/// before the operation.
fn classify_dimension(requested: f64, applied: f64, previous: Option<f64>) -> DimensionOutcome {
    if approx_equal(applied, requested) {
        DimensionOutcome::Applied
    } else if previous.is_some_and(|previous| approx_equal(applied, previous)) {
        DimensionOutcome::Frozen
    } else {
        DimensionOutcome::Clamped
    }
}

/// Classify the origin as a whole (both coordinates must match for `Applied`).
fn classify_position(
    requested: Point,
    applied: Point,
    previous: Option<Point>,
) -> DimensionOutcome {
    let applied_ok = approx_equal(applied.x, requested.x) && approx_equal(applied.y, requested.y);
    if applied_ok {
        return DimensionOutcome::Applied;
    }
    if previous.is_some_and(|previous| {
        approx_equal(applied.x, previous.x) && approx_equal(applied.y, previous.y)
    }) {
        DimensionOutcome::Frozen
    } else {
        DimensionOutcome::Clamped
    }
}

fn classify_axis(original: f64, probe: f64, applied: f64) -> AxisConstraint {
    if approx_equal(applied, probe) {
        AxisConstraint::Free
    } else if approx_equal(applied, original) {
        AxisConstraint::Frozen { value: original }
    } else {
        AxisConstraint::Min {
            min: applied.max(0.0),
        }
    }
}

fn merge_max(
    width: AxisConstraint,
    height: AxisConstraint,
    declared_max: Option<Size>,
) -> Option<Size> {
    let width_max = match (width.maximum(), declared_max.map(|size| size.width)) {
        (Some(frozen), Some(declared)) => Some(frozen.min(declared)),
        (Some(frozen), None) => Some(frozen),
        (None, Some(declared)) => Some(declared),
        (None, None) => None,
    };
    let height_max = match (height.maximum(), declared_max.map(|size| size.height)) {
        (Some(frozen), Some(declared)) => Some(frozen.min(declared)),
        (Some(frozen), None) => Some(frozen),
        (None, Some(declared)) => Some(declared),
        (None, None) => None,
    };
    match (width_max, height_max) {
        (None, None) => None,
        (width, height) => Some(Size::new(
            width.unwrap_or(f64::MAX),
            height.unwrap_or(f64::MAX),
        )),
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
    /// then the position, then the size again converges for those windows. After
    /// one corrective pass the frame is read back **again** and returned, so the
    /// caller can see what the application actually applied instead of assuming
    /// the request succeeded.
    pub fn set_frame(&self, frame: Rect) -> Result<AppliedFrame> {
        let previous = self.frame()?;
        self.element.set_size_attribute(ATTR_SIZE, frame.size)?;
        self.element
            .set_point_attribute(ATTR_POSITION, frame.origin)?;
        self.element.set_size_attribute(ATTR_SIZE, frame.size)?;

        // Read back. AppKit can clamp a size after the move (screen edges,
        // aspect ratio); one corrective pass is enough in practice.
        let mut applied = self.current_frame()?;
        if frame_differs(applied, frame) {
            log::debug!(
                "set_frame({}) requested {:?}, got {:?}; retrying",
                self.wid,
                frame,
                applied
            );
            self.element
                .set_point_attribute(ATTR_POSITION, frame.origin)?;
            self.element.set_size_attribute(ATTR_SIZE, frame.size)?;
            applied = self.current_frame()?;
        }
        Ok(AppliedFrame::from_frames(frame, applied, previous))
    }

    /// Move without resizing.
    ///
    /// Returns the frame the application actually applied; a window that refuses
    /// to move reports [`DimensionOutcome::Frozen`] for its position.
    pub fn set_position(&self, point: Point) -> Result<AppliedFrame> {
        let previous = self.frame()?;
        self.element.set_point_attribute(ATTR_POSITION, point)?;
        let applied = self.current_frame()?;
        let size = previous.unwrap_or(applied).size;
        let requested = Rect::new(point.x, point.y, size.width, size.height);
        Ok(AppliedFrame::from_frames(requested, applied, previous))
    }

    /// Resize without moving.
    ///
    /// Returns the frame the application actually applied.  This is how a caller
    /// detects a silently clamped dimension.
    pub fn set_size(&self, size: Size) -> Result<AppliedFrame> {
        let previous = self.frame()?;
        self.element.set_size_attribute(ATTR_SIZE, size)?;
        let applied = self.current_frame()?;
        let origin = previous.unwrap_or(applied).origin;
        let requested = Rect::new(origin.x, origin.y, size.width, size.height);
        Ok(AppliedFrame::from_frames(requested, applied, previous))
    }

    /// Discover the minimum (and, when the app exposes it, maximum) size.
    ///
    /// **This momentarily resizes the window** — it shrinks the window to 1×1 for
    /// roughly 50–100 ms, reads back what the application actually allowed, and
    /// restores the original frame.  The platform layer never calls this
    /// automatically; the daemon decides when a probe is worth the visible
    /// flicker (for example once after a window is first managed, or after a
    /// `SetFrame` was clamped).
    ///
    /// Applications can respond in three ways per dimension, reported through
    /// [`AxisConstraint`]:
    ///
    /// * the requested probe size is applied → [`AxisConstraint::Free`];
    /// * something between the probe and the original is applied → the application
    ///   has a minimum on that axis ([`AxisConstraint::Min`]);
    /// * the original size is unchanged → the dimension is frozen
    ///   ([`AxisConstraint::Frozen`]), which System Settings does with its width.
    ///
    /// A maximum is read directly from `AXMaxSize` / `AXMaximumSize` when the
    /// application publishes it; the probe never grows a window to find a maximum.
    pub fn probe_size_constraints(&self) -> Result<SizeConstraints> {
        if !crate::ax::is_process_trusted() {
            return Err(PlatformError::AccessibilityDenied);
        }
        let original = self.current_frame()?;
        let declared_max = self.declared_max_size();
        let probe = Size::new(
            if original.size.width > PROBE_SIZE + FRAME_TOLERANCE {
                PROBE_SIZE
            } else {
                original.size.width
            },
            if original.size.height > PROBE_SIZE + FRAME_TOLERANCE {
                PROBE_SIZE
            } else {
                original.size.height
            },
        );
        if probe == original.size {
            // Nothing to learn without growing the window.
            return Ok(SizeConstraints::unconstrained(declared_max));
        }

        let applied = self.set_frame(Rect::new(
            original.origin.x,
            original.origin.y,
            probe.width,
            probe.height,
        ))?;
        // Always try to restore, even if the probe itself looked odd.
        match self.set_frame(original) {
            Ok(restored) => {
                if restored.size_clamped() {
                    log::debug!(
                        "probe({}): restore requested {:?}, got {:?}",
                        self.wid,
                        original,
                        restored.applied
                    );
                }
            }
            Err(err) => log::warn!("probe({}): could not restore frame: {err}", self.wid),
        }
        Ok(SizeConstraints::from_probe(
            original.size,
            probe,
            applied.applied.size,
            declared_max,
        ))
    }

    /// Read an application-declared maximum size, if one of the known attribute
    /// spellings is supported.
    fn declared_max_size(&self) -> Option<Size> {
        for name in [ATTR_MAX_SIZE, ATTR_MAXIMUM_SIZE] {
            if let Ok(Some(size)) = self.element.size_attribute(name)
                && size.width > 0.0
                && size.height > 0.0
            {
                return Some(size);
            }
        }
        None
    }

    /// Read the current frame or report that the window has no AX geometry.
    fn current_frame(&self) -> Result<Rect> {
        self.frame()?.ok_or_else(|| {
            PlatformError::Other(format!("window {} has no AXPosition/AXSize", self.wid))
        })
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

/// Move and resize a window, returning the frame the application actually applied.
pub fn set_frame(wid: u32, frame: Rect) -> Result<AppliedFrame> {
    ax_window(wid)?.set_frame(frame)
}

/// Move a window without resizing, returning the applied frame.
pub fn set_position(wid: u32, point: Point) -> Result<AppliedFrame> {
    ax_window(wid)?.set_position(point)
}

/// Resize a window without moving it, returning the applied frame.
pub fn set_size(wid: u32, size: Size) -> Result<AppliedFrame> {
    ax_window(wid)?.set_size(size)
}

fn constraint_cache() -> &'static Mutex<HashMap<u32, SizeConstraints>> {
    static CACHE: OnceLock<Mutex<HashMap<u32, SizeConstraints>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Probe a window's size constraints, caching the result per window id.
///
/// **The first call momentarily resizes the window** (see
/// [`AxWindow::probe_size_constraints`]).  Subsequent calls return the cached
/// result until [`forget_size_constraints`] or [`clear_size_constraint_cache`]
/// is called.  The cache is process-local and therefore reset when the daemon
/// restarts.
pub fn probe_size_constraints(wid: u32) -> Result<Option<SizeConstraints>> {
    if let Some(cached) = constraint_cache()
        .lock()
        .expect("constraint cache poisoned")
        .get(&wid)
        .copied()
    {
        return Ok(Some(cached));
    }
    let constraints = ax_window(wid)?.probe_size_constraints()?;
    constraint_cache()
        .lock()
        .expect("constraint cache poisoned")
        .insert(wid, constraints);
    Ok(Some(constraints))
}

/// The window's minimum accepted size, probing only when not cached.
///
/// `None` means no minimum was detected.  See [`probe_size_constraints`] for the
/// momentary resize this may perform.
pub fn min_size(wid: u32) -> Result<Option<Size>> {
    Ok(probe_size_constraints(wid)?.and_then(|constraints| constraints.min))
}

/// The window's maximum accepted size, if the application declares one.
pub fn max_size(wid: u32) -> Result<Option<Size>> {
    Ok(probe_size_constraints(wid)?.and_then(|constraints| constraints.max))
}

/// Drop a cached probe result (for example after the window was resized by the
/// user or its app changed constraints).
pub fn forget_size_constraints(wid: u32) {
    constraint_cache()
        .lock()
        .expect("constraint cache poisoned")
        .remove(&wid);
}

/// Drop every cached probe result.
pub fn clear_size_constraint_cache() {
    constraint_cache()
        .lock()
        .expect("constraint cache poisoned")
        .clear();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_classification_distinguishes_applied_clamped_and_frozen() {
        // Exact request honoured.
        assert_eq!(
            classify_dimension(800.0, 800.0, Some(500.0)),
            DimensionOutcome::Applied
        );
        // Application clamped to a different value.
        assert_eq!(
            classify_dimension(800.0, 642.0, Some(500.0)),
            DimensionOutcome::Clamped
        );
        // Application ignored the request: value stayed where it was.
        assert_eq!(
            classify_dimension(800.0, 715.0, Some(715.0)),
            DimensionOutcome::Frozen
        );
        // Sub-pixel differences are still "applied".
        assert_eq!(
            classify_dimension(833.0, 833.4, Some(700.0)),
            DimensionOutcome::Applied
        );
    }

    #[test]
    fn applied_frame_reports_per_axis_outcome_and_clamped_flag() {
        let requested = Rect::new(100.0, 50.0, 800.0, 600.0);
        let previous = Rect::new(10.0, 20.0, 700.0, 500.0);
        let applied = Rect::new(100.0, 50.0, 642.0, 600.0);
        let result = AppliedFrame::from_frames(requested, applied, Some(previous));
        assert_eq!(result.width, DimensionOutcome::Clamped);
        assert_eq!(result.height, DimensionOutcome::Applied);
        assert_eq!(result.position, DimensionOutcome::Applied);
        assert!(result.clamped);
        assert!(result.size_clamped());
        assert!(!result.position_clamped());
        assert!(!result.matches_requested());
    }

    #[test]
    fn applied_frame_reports_frozen_position() {
        let requested = Rect::new(100.0, 50.0, 800.0, 600.0);
        let previous = Rect::new(10.0, 20.0, 800.0, 600.0);
        let applied = Rect::new(10.0, 20.0, 800.0, 600.0);
        let result = AppliedFrame::from_frames(requested, applied, Some(previous));
        assert_eq!(result.position, DimensionOutcome::Frozen);
        assert!(result.position_clamped());
        assert!(!result.size_clamped());
    }

    #[test]
    fn probe_logic_detects_free_min_and_frozen_axes() {
        let original = Size::new(800.0, 600.0);
        let probe = Size::new(1.0, 1.0);

        let free = SizeConstraints::from_probe(original, probe, probe, None);
        assert_eq!(free.width, AxisConstraint::Free);
        assert_eq!(free.height, AxisConstraint::Free);
        assert_eq!(free.min, None);
        assert_eq!(free.max, None);

        let width_min = SizeConstraints::from_probe(original, probe, Size::new(642.0, 1.0), None);
        assert_eq!(width_min.width, AxisConstraint::Min { min: 642.0 });
        assert_eq!(width_min.height, AxisConstraint::Free);
        assert_eq!(width_min.min, Some(Size::new(642.0, 0.0)));

        let frozen = SizeConstraints::from_probe(
            Size::new(715.0, 600.0),
            probe,
            Size::new(715.0, 1.0),
            None,
        );
        assert_eq!(frozen.width, AxisConstraint::Frozen { value: 715.0 });
        assert_eq!(frozen.min, Some(Size::new(715.0, 0.0)));
        assert_eq!(frozen.max, Some(Size::new(715.0, f64::MAX)));
    }

    #[test]
    fn declared_maximum_is_used_without_growing_the_window() {
        let original = Size::new(800.0, 600.0);
        let probe = Size::new(1.0, 1.0);
        let declared = Size::new(4000.0, 3000.0);
        let constraints = SizeConstraints::from_probe(original, probe, probe, Some(declared));
        assert_eq!(constraints.max, Some(declared));
        assert_eq!(constraints.min, None);

        // A frozen axis is at most its frozen value, even if AXMaxSize is larger.
        let frozen = SizeConstraints::from_probe(
            original,
            probe,
            Size::new(800.0, 1.0),
            Some(Size::new(4000.0, 3000.0)),
        );
        assert_eq!(frozen.max, Some(Size::new(800.0, 3000.0)));
    }

    #[test]
    fn probe_target_equal_to_current_size_reports_no_constraint() {
        let original = Size::new(1.0, 1.0);
        let constraints = SizeConstraints::from_probe(original, original, original, None);
        assert!(constraints.width.is_free());
        assert!(constraints.height.is_free());
        assert_eq!(constraints.min, None);
    }

    #[test]
    fn axis_constraint_accessors() {
        assert_eq!(AxisConstraint::Free.minimum(), 0.0);
        assert_eq!(AxisConstraint::Free.maximum(), None);
        assert!(AxisConstraint::Free.is_free());
        assert_eq!(AxisConstraint::Min { min: 200.0 }.minimum(), 200.0);
        assert_eq!(AxisConstraint::Min { min: 200.0 }.maximum(), None);
        assert_eq!(AxisConstraint::Frozen { value: 715.0 }.minimum(), 715.0);
        assert_eq!(
            AxisConstraint::Frozen { value: 715.0 }.maximum(),
            Some(715.0)
        );
    }
}
