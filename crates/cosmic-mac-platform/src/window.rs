//! Window model, enumeration and imperative window operations.
//!
//! Enumeration uses the public `CGWindowListCopyWindowInfo` as the source of
//! truth for *what is on screen* (window IDs, stacking layer, bounds, owner
//! PID). When the Accessibility grant is present, each window is enriched with
//! AX data: title, role/subrole, resizability, dialog-ness and the
//! minimized/full-screen state.
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
    ACTION_PRESS, ACTION_RAISE, ATTR_CLOSE_BUTTON, ATTR_FULLSCREEN, ATTR_MAX_SIZE,
    ATTR_MAXIMUM_SIZE, ATTR_MINIMIZED, ATTR_POSITION, ATTR_ROLE, ATTR_SIZE, ATTR_SUBROLE,
    ATTR_TITLE, ATTR_WINDOWS, AxElement,
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
    /// In native full screen (AXFullScreen): the window fills its own Space.
    pub is_fullscreen: bool,
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
        self.level == 0 && !self.is_minimized && !self.is_fullscreen
    }
}

/// How an application responded to one requested dimension of a geometry change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DimensionOutcome {
    /// The application applied the requested value (within [`FRAME_TOLERANCE`]).
    Applied,
    /// The application applied a different value: it has a minimum or maximum on this axis.
    Clamped,
    /// The application ignored this particular change: the value stayed where it was.
    ///
    /// This is **not** proof that the dimension is permanently frozen.  A single request
    /// cannot tell a minimum equal to the current size (the app refuses to shrink but can
    /// grow) from a maximum or a genuinely frozen dimension.  Use
    /// [`AxWindow::probe_size_constraints`] to distinguish those cases.
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
    /// The axis followed both the shrink and grow probes; no minimum was detected.
    #[default]
    Free,
    /// The application refused to shrink below this value, but can still grow
    /// (unless a grow clamp also produced a real maximum).
    Min {
        /// Smallest accepted value in logical pixels.
        min: f64,
    },
    /// The application refused both the shrink and the grow probe; the dimension is
    /// genuinely stuck at this value, so the minimum and maximum are both `value`.
    Frozen {
        /// The value the dimension is stuck at.
        value: f64,
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
    /// Build a result for a window whose probe targets equalled its current size.
    fn unconstrained(max: Option<Size>) -> Self {
        SizeConstraints {
            min: None,
            max,
            width: AxisConstraint::Free,
            height: AxisConstraint::Free,
        }
    }

    /// Pure logic behind the probe: classify the shrink and grow results per axis and merge
    /// any declared maximum.
    ///
    /// `original` is the size before probing, `shrink` / `grow` are the requested probe sizes
    /// and `applied_shrink` / `applied_grow` are what the application actually produced.
    /// `grow` is `None` when the probe could not grow the window (no display cap), in which
    /// case the classification degrades to shrink-only.
    fn from_probes(
        original: Size,
        shrink: Size,
        applied_shrink: Size,
        grow: Option<Size>,
        applied_grow: Option<Size>,
        declared_max: Option<Size>,
    ) -> Self {
        let width = classify_axis(
            original.width,
            shrink.width,
            applied_shrink.width,
            grow.map(|size| size.width),
            applied_grow.map(|size| size.width),
        );
        let height = classify_axis(
            original.height,
            shrink.height,
            applied_shrink.height,
            grow.map(|size| size.height),
            applied_grow.map(|size| size.height),
        );
        let min = if width.constraint.is_free() && height.constraint.is_free() {
            None
        } else {
            Some(Size::new(
                width.constraint.minimum(),
                height.constraint.minimum(),
            ))
        };
        let max = merge_max(width.max, height.max, declared_max);
        SizeConstraints {
            min,
            max,
            width: width.constraint,
            height: height.constraint,
        }
    }
}

/// Tolerance used when comparing requested and applied geometry, in logical pixels.
pub const FRAME_TOLERANCE: f64 = 1.0;

/// Size the probe shrinks to before reading back the application's minimum.
const SHRINK_PROBE_SIZE: f64 = 1.0;

/// How far the probe grows a window to detect a maximum.
const GROW_STEP: f64 = 128.0;

/// Largest grow shortfall that is read as resize-grid snapping rather than a maximum.
///
/// Apps with a resize increment answer a grow with the nearest cell: Terminal and iTerm
/// snap to their character cell (7×14 px at Terminal's default font), so a +128 px grow
/// comes back a few pixels short.  Recording that as a maximum pinned Terminal a few pixels
/// above its current size for the session.  A real maximum refuses most of the step, so only
/// a shortfall of more than half of [`GROW_STEP`] counts; that also leaves room for apps with
/// coarser increments.  A real maximum closer than that is not lost: the daemon learns it
/// when its own clamps stop at the same value from two different requests.
const GROW_SNAP_LIMIT: f64 = GROW_STEP / 2.0;

/// The grow probe is capped at this fraction of the main display's frame, so it never asks
/// for an enormous window that the display would clamp for unrelated reasons.
const GROW_DISPLAY_FRACTION: f64 = 0.5;

/// The shrink target: 1×1, or the current size when an axis is already tiny.
fn shrink_target(original: Size) -> Size {
    Size::new(
        if original.width > SHRINK_PROBE_SIZE + FRAME_TOLERANCE {
            SHRINK_PROBE_SIZE
        } else {
            original.width
        },
        if original.height > SHRINK_PROBE_SIZE + FRAME_TOLERANCE {
            SHRINK_PROBE_SIZE
        } else {
            original.height
        },
    )
}

/// The grow target, or `None` when the display cap cannot be queried or leaves no room to
/// grow (the caller then degrades to shrink-only classification).
fn grow_target(original: Size, cap: Option<Size>) -> Option<Size> {
    let cap = cap?;
    let width = (original.width + GROW_STEP)
        .min(cap.width)
        .max(original.width);
    let height = (original.height + GROW_STEP)
        .min(cap.height)
        .max(original.height);
    if width <= original.width + FRAME_TOLERANCE && height <= original.height + FRAME_TOLERANCE {
        None
    } else {
        Some(Size::new(width, height))
    }
}

/// Half of the main display's frame plus a safe origin for the grow probe.
///
/// The original window position is deliberately not reused: a window near the
/// bottom or right edge would be clamped by the display rather than by the
/// application, which the caller would otherwise record as a real maximum.
fn grow_cap() -> Option<(Size, Point)> {
    // The WM thread must not enumerate displays itself (NSScreen is main-thread
    // only); use the snapshot the main thread keeps current.
    let display =
        crate::display::cached_main_display().or_else(|| crate::display::main_display().ok())?;
    let cap = Size::new(
        display.frame.size.width * GROW_DISPLAY_FRACTION,
        display.frame.size.height * GROW_DISPLAY_FRACTION,
    );
    if cap.width <= 0.0 || cap.height <= 0.0 {
        return None;
    }
    Some((cap, display.visible_frame.origin))
}

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

/// One axis finding: the shrink-side constraint plus any grow-side maximum.
struct AxisFinding {
    constraint: AxisConstraint,
    max: Option<f64>,
}

/// Classify one axis from a shrink probe and an optional grow probe.
///
/// `Frozen` requires **both** probes to be refused.  A window whose minimum equals its
/// natural size refuses to shrink but grows normally, which is [`AxisConstraint::Min`], not
/// frozen.  A grow that falls more than [`GROW_SNAP_LIMIT`] short of the request is reported
/// as a real maximum even when the axis has no minimum; a smaller shortfall is grid snapping.
/// When the grow probe could not run, a refused shrink degrades to
/// [`AxisConstraint::Frozen`] (the old shrink-only behaviour) because the two cases cannot be
/// distinguished without growing the window.
fn classify_axis(
    original: f64,
    shrink: f64,
    applied_shrink: f64,
    grow: Option<f64>,
    applied_grow: Option<f64>,
) -> AxisFinding {
    let shrink_probed = shrink < original - FRAME_TOLERANCE;
    let shrink_refused = shrink_probed && approx_equal(applied_shrink, original);
    let min = if shrink_probed && applied_shrink > shrink + FRAME_TOLERANCE {
        Some(applied_shrink.max(0.0))
    } else {
        None
    };

    // Without a completed grow probe we cannot tell a natural-size minimum from a frozen
    // dimension; keep the conservative shrink-only answer.
    let Some(grow) = grow.filter(|grow| *grow > original + FRAME_TOLERANCE) else {
        return shrink_only_finding(original, shrink_refused, min);
    };
    let Some(applied_grow) = applied_grow else {
        return shrink_only_finding(original, shrink_refused, min);
    };

    let grow_refused = approx_equal(applied_grow, original);
    if shrink_refused && grow_refused {
        return AxisFinding {
            constraint: AxisConstraint::Frozen { value: original },
            max: Some(original),
        };
    }

    // Only a grow that fell well short of the request is a real maximum.  An application that
    // snaps the grow to its resize grid lands within a cell of the request, and one that snaps
    // it upward is not constraining the axis at all.
    let max =
        (grow - applied_grow > GROW_SNAP_LIMIT).then_some(applied_grow.max(original).max(0.0));
    let constraint = min
        .map(|min| AxisConstraint::Min { min })
        .unwrap_or(AxisConstraint::Free);
    AxisFinding { constraint, max }
}

/// Result when the grow probe could not run: a refused shrink is conservatively `Frozen`.
fn shrink_only_finding(original: f64, shrink_refused: bool, min: Option<f64>) -> AxisFinding {
    if shrink_refused {
        AxisFinding {
            constraint: AxisConstraint::Frozen { value: original },
            max: Some(original),
        }
    } else {
        AxisFinding {
            constraint: min
                .map(|min| AxisConstraint::Min { min })
                .unwrap_or(AxisConstraint::Free),
            max: None,
        }
    }
}

fn merge_max(
    width_max: Option<f64>,
    height_max: Option<f64>,
    declared_max: Option<Size>,
) -> Option<Size> {
    let width_max = combine_max(width_max, declared_max.map(|size| size.width));
    let height_max = combine_max(height_max, declared_max.map(|size| size.height));
    match (width_max, height_max) {
        (None, None) => None,
        (width, height) => Some(Size::new(
            width.unwrap_or(f64::MAX),
            height.unwrap_or(f64::MAX),
        )),
    }
}

fn combine_max(probed: Option<f64>, declared: Option<f64>) -> Option<f64> {
    match (probed, declared) {
        (Some(probed), Some(declared)) => Some(probed.min(declared)),
        (Some(probed), None) => Some(probed),
        (None, Some(declared)) => Some(declared),
        (None, None) => None,
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

/// The cheap fields of one CGWindowList record: no LaunchServices or AX
/// lookups.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CgWindowBasics {
    wid: u32,
    pid: libc::pid_t,
    level: i32,
    frame: Rect,
}

impl CgWindowBasics {
    /// The [`list_windows_all_levels`] filter: a real window of another
    /// process with a non-empty frame, at any level.
    fn is_foreign_window(&self, own_pid: libc::pid_t) -> bool {
        self.wid != 0 && self.pid != own_pid && self.frame.is_non_empty()
    }

    /// The [`list_windows`] filter: a real window of another process, at the
    /// normal application level, with a non-empty frame.
    fn is_listed(&self, own_pid: libc::pid_t) -> bool {
        self.is_foreign_window(own_pid) && self.level == 0
    }
}

fn own_pid() -> libc::pid_t {
    std::process::id() as libc::pid_t
}

fn cg_dict_basics(dict: &CFDictionary<CFString, CFType>) -> CgWindowBasics {
    // SAFETY: the kCGWindow* statics are immutable CFStrings owned by
    // CoreGraphics and valid for the process lifetime.
    let (key_number, key_pid, key_layer, key_bounds) = unsafe {
        (
            kCGWindowNumber,
            kCGWindowOwnerPID,
            kCGWindowLayer,
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

    CgWindowBasics {
        wid,
        pid,
        level,
        frame,
    }
}

fn cg_dict_to_window(dict: &CFDictionary<CFString, CFType>) -> WindowInfo {
    // SAFETY: the kCGWindow* statics are immutable CFStrings owned by
    // CoreGraphics and valid for the process lifetime.
    let (key_onscreen, key_name, key_owner) =
        unsafe { (kCGWindowIsOnscreen, kCGWindowName, kCGWindowOwnerName) };
    let CgWindowBasics {
        wid,
        pid,
        level,
        frame,
    } = cg_dict_basics(dict);
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
        is_fullscreen: false,
        ax_backed: false,
    }
}

/// Call `visit` for every on-screen CGWindowList record (desktop elements
/// excluded), front to back.
fn for_each_on_screen_record(mut visit: impl FnMut(&CFDictionary<CFString, CFType>)) -> Result<()> {
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
    for index in 0..array.len() {
        let Some(value) = array.get(index) else {
            continue;
        };
        let Ok(dict) = value.downcast::<CFDictionary>() else {
            continue;
        };
        // SAFETY: string-keyed dictionaries.
        let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
        visit(dict);
    }
    Ok(())
}

/// All on-screen windows at normal application level.
///
/// This works without Accessibility. Titles and roles will be `None` and
/// `resizable` defaults to `true` unless the process is trusted.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let own_pid = own_pid();
    let mut windows = Vec::new();
    for_each_on_screen_record(|dict| {
        if cg_dict_basics(dict).is_listed(own_pid) {
            windows.push(cg_dict_to_window(dict));
        }
    })?;

    if crate::ax::is_process_trusted() {
        enrich_with_ax(&mut windows);
    }
    Ok(windows)
}

/// All on-screen windows of other processes at **any** level, front to back.
///
/// Same enumeration as [`list_windows`] without the level filter, from
/// CGWindowList data only: no AX enrichment, so nothing here talks to the
/// applications and a hung app cannot stall the caller.  Roles and the
/// minimized/full-screen flags are not filled in, and titles are CoreGraphics
/// titles only.  The owning app's bundle id and name come from
/// `NSRunningApplication`.
/// Used to find shell windows (the panel at the status level) that are
/// already on screen.
pub fn list_windows_all_levels() -> Result<Vec<WindowInfo>> {
    let own_pid = own_pid();
    let mut windows = Vec::new();
    for_each_on_screen_record(|dict| {
        if cg_dict_basics(dict).is_foreign_window(own_pid) {
            windows.push(cg_dict_to_window(dict));
        }
    })?;
    Ok(windows)
}

/// Owner pids of the windows [`list_windows`] would return, sorted and
/// deduplicated.
///
/// Same enumeration and filters, but **no** AX enrichment and no
/// LaunchServices lookup: nothing here talks to the applications, so a hung
/// app cannot stall the caller.  Used for the periodic observer refresh.
pub fn list_window_pids() -> Result<Vec<libc::pid_t>> {
    let mut windows = Vec::new();
    for_each_on_screen_record(|dict| windows.push(cg_dict_basics(dict)))?;
    Ok(listed_pids(windows, own_pid()))
}

fn listed_pids(
    windows: impl IntoIterator<Item = CgWindowBasics>,
    own_pid: libc::pid_t,
) -> Vec<libc::pid_t> {
    let mut pids: Vec<libc::pid_t> = windows
        .into_iter()
        .filter(|window| window.is_listed(own_pid))
        .map(|window| window.pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
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
            window.is_fullscreen = is_fullscreen(&element);
            window.resizable = element.is_settable(ATTR_SIZE).unwrap_or(true);
            window.is_dialog = is_dialog_role(window.role.as_deref(), window.subrole.as_deref());
            window.is_floating = window.subrole.as_deref() == Some("AXFloatingWindow");
            if let Ok(Some(frame)) = element.frame() {
                window.frame = frame;
            }
        }
    }
}

/// Whether a window element is in native full screen (`AXFullScreen`).
///
/// Windows that cannot go full screen do not publish the attribute; that, and
/// a failed read, count as not full screen.
fn is_fullscreen(element: &AxElement) -> bool {
    element
        .bool_attribute(ATTR_FULLSCREEN)
        .ok()
        .flatten()
        .unwrap_or(false)
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
            is_fullscreen: is_fullscreen(&element),
            ax_backed: true,
        });
    }
    Ok(windows)
}

/// The CoreGraphics bounds of `wid`, from its `CGWindowList` record
/// (`kCGWindowListOptionIncludingWindow`: any Space, on screen or not), or
/// `None` when CoreGraphics does not list the window.  No Accessibility.
pub fn cg_window_frame(wid: u32) -> Option<Rect> {
    let options =
        CGWindowListOption::OptionIncludingWindow | CGWindowListOption::ExcludeDesktopElements;
    let array = CGWindowListCopyWindowInfo(options, wid)?;
    // SAFETY: CGWindowListCopyWindowInfo returns an array of CFDictionary.
    let array: &CFArray<CFType> = unsafe { array.cast_unchecked() };
    (0..array.len())
        .filter_map(|index| array.get(index))
        .filter_map(|value| value.downcast::<CFDictionary>().ok())
        .map(|dict| {
            // SAFETY: CG window dictionaries are string-keyed.
            let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
            cg_dict_basics(dict)
        })
        .find(|basics| basics.wid == wid)
        .map(|basics| basics.frame)
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
        info.is_fullscreen = is_fullscreen(&window.element);
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
        self.set_size_best_effort(frame.size);
        self.element
            .set_point_attribute(ATTR_POSITION, frame.origin)?;
        self.set_size_best_effort(frame.size);

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
            self.set_size_best_effort(frame.size);
            applied = self.current_frame()?;
        }
        Ok(AppliedFrame::from_frames(frame, applied, previous))
    }

    /// Set the size, tolerating a refusal.
    ///
    /// Some windows reject `AXSize` outright (Calculator answers
    /// `kAXErrorFailure` even though the attribute reports settable).  Failing
    /// the whole `set_frame` then left such a window unmoved: never placed,
    /// never restored from the journal.  The position is still applied, and the
    /// read-back reports the refused size as a clamp.
    fn set_size_best_effort(&self, size: Size) {
        if let Err(error) = self.element.set_size_attribute(ATTR_SIZE, size) {
            log::debug!("window {}: size {size:?} refused: {error}", self.wid);
        }
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

    /// Discover the minimum and maximum size the application accepts.
    ///
    /// **This momentarily resizes the window** — it shrinks the window to 1×1, reads
    /// back what the application allowed, then grows it by [`GROW_STEP`] on each
    /// axis (capped at [`GROW_DISPLAY_FRACTION`] of the main display), reads that
    /// back too, and restores the original frame after each step.  The grow step
    /// runs at the main display's visible origin so a window near a screen edge
    /// does not look like it has a maximum.  The platform layer never calls this
    /// automatically; the daemon decides when a probe is worth the visible flicker
    /// (for example after a `SetFrame` was clamped).
    ///
    /// The two steps are what tell a **minimum equal to the current size** apart
    /// from a **frozen** dimension.  Applications respond per axis:
    ///
    /// * shrink applied fully **and** grow applied fully → [`AxisConstraint::Free`];
    /// * the shrink stopped between 1×1 and the original size → the application
    ///   has that minimum ([`AxisConstraint::Min`]);
    /// * the shrink returned to the original size **and** the grow was rejected →
    ///   the dimension is frozen ([`AxisConstraint::Frozen`]), which System
    ///   Settings does with its width;
    /// * the shrink returned to the original size **and** the grow was applied →
    ///   the minimum equals the natural size ([`AxisConstraint::Min`]), which is
    ///   what Notes' 642px width is: it refuses to shrink but widens happily.
    ///
    /// A grow that stops more than [`GROW_SNAP_LIMIT`] short of the requested
    /// size is a **maximum**; it is reported through [`SizeConstraints::max`]
    /// together with `AXMaxSize` / `AXMaximumSize` when the application
    /// publishes one.  A grow that stops only a few pixels short is the app
    /// snapping to its resize grid (Terminal), not a maximum.
    ///
    /// If the main display cannot be queried the grow step is skipped and the
    /// probe degrades to the shrink-only behaviour: in that case a minimum equal
    /// to the natural size is reported as [`AxisConstraint::Frozen`], because the
    /// two cannot be distinguished without growing the window.
    pub fn probe_size_constraints(&self) -> Result<SizeConstraints> {
        if !crate::ax::is_process_trusted() {
            return Err(PlatformError::AccessibilityDenied);
        }
        let original = self.current_frame()?;
        let declared_max = self.declared_max_size();
        let shrink = shrink_target(original.size);
        let grow_probe = grow_cap();
        let grow = grow_target(original.size, grow_probe.map(|(cap, _)| cap));
        if shrink == original.size && grow.is_none() {
            // Nothing to learn without growing the window.
            return Ok(SizeConstraints::unconstrained(declared_max));
        }

        let shrink_applied = self.probe_once(original, shrink, original.origin)?;
        let grow_applied = match grow {
            Some(grow) => {
                // Grow at the display's visible origin, not at the window's
                // current position: growing downward/rightward from a window
                // near an edge would otherwise be clamped by the screen and
                // misreported as an application maximum.
                let origin = grow_probe
                    .map(|(_, origin)| origin)
                    .unwrap_or(original.origin);
                match self.probe_once(original, grow, origin) {
                    Ok(applied) => Some(applied),
                    Err(error) => {
                        // A failed grow step must not lose the shrink result.
                        log::debug!("probe({}): grow step failed: {error}", self.wid);
                        None
                    }
                }
            }
            None => None,
        };
        Ok(SizeConstraints::from_probes(
            original.size,
            shrink,
            shrink_applied,
            grow,
            grow_applied,
            declared_max,
        ))
    }

    /// Apply one probe size, read back the size the application produced, and
    /// always restore the original frame.
    fn probe_once(&self, original: Rect, probe: Size, origin: Point) -> Result<Size> {
        let applied = self.set_frame(Rect::new(origin.x, origin.y, probe.width, probe.height))?;
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
        Ok(applied.applied.size)
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

/// How far, in points, a window's top-left corner may sit up or left of
/// [`park_origin`] and still count as parked for [`is_parked_frame`].
///
/// [`ParkStore::park`] puts the corner exactly at the park origin; the slack
/// absorbs read-back rounding and small nudges by the app or macOS.  A window
/// that far into the bottom-right corner shows at most a 9×9 pt sliver, which
/// is no usable position, so a false positive costs nothing.  Missing a parked
/// window is expensive: the daemon would journal the corner as the window's
/// original frame and park it for good on shutdown.
pub const PARKED_FRAME_SLACK: f64 = 8.0;

/// Where [`ParkStore::park`] moves a window's top-left corner on
/// `display_frame`: 1 px inside the display's bottom-right corner, so 1 px of
/// the window stays visible.
pub fn park_origin(display_frame: Rect) -> Point {
    Point::new(display_frame.max_x() - 1.0, display_frame.max_y() - 1.0)
}

/// Whether `frame` is where [`ParkStore::park`] leaves a window on
/// `display_frame`: its top-left corner is at [`park_origin`] (within
/// [`PARKED_FRAME_SLACK`]) or further right and down, off the display (the
/// display shrank since the window was parked).
///
/// An empty `display_frame` cannot be judged and yields `false`.
pub fn is_parked_frame(frame: Rect, display_frame: Rect) -> bool {
    if !display_frame.is_non_empty() {
        return false;
    }
    let origin = park_origin(display_frame);
    frame.origin.x >= origin.x - PARKED_FRAME_SLACK
        && frame.origin.y >= origin.y - PARKED_FRAME_SLACK
}

/// Saved frames for parked windows.
///
/// The WM parks inactive workspace members and inactive stack members at the
/// bottom-right corner of their display with 1 px visible (see
/// [`park_origin`]). This store keeps the original frame so `unpark` can
/// restore it exactly. It is in-memory; the daemon's crash journal is a
/// separate concern.
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
        set_position(wid, park_origin(display_frame))?;
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
    fn parked_frames_are_recognised_at_the_park_origin() {
        let display = Rect::new(0.0, 0.0, 1728.0, 1080.0);
        let parked =
            |x: f64, y: f64, display: Rect| is_parked_frame(Rect::new(x, y, 800.0, 600.0), display);
        assert_eq!(park_origin(display), Point::new(1727.0, 1079.0));
        // Exactly where `park` leaves it, and a few points off.
        assert!(parked(1727.0, 1079.0, display));
        assert!(parked(1720.0, 1072.0, display));
        // Beyond the corner: parked on a display that has since shrunk.
        assert!(parked(1919.0, 1199.0, display));
        // A real window, one in the bottom-right quarter and ones that only
        // touch a single edge are not parked.
        assert!(!parked(100.0, 100.0, display));
        assert!(!parked(1200.0, 700.0, display));
        assert!(!parked(1727.0, 500.0, display));
        assert!(!parked(500.0, 1079.0, display));
        // A secondary display's corner is judged against that display.
        let right = Rect::new(1728.0, 0.0, 1920.0, 1200.0);
        assert!(parked(3647.0, 1199.0, right));
        assert!(!parked(1727.0, 1079.0, right));
        // An empty display frame cannot be judged.
        assert!(!parked(1727.0, 1079.0, Rect::default()));
    }

    #[test]
    fn listed_pids_apply_the_list_windows_filter_and_dedup() {
        let window = |wid: u32, pid: libc::pid_t, level: i32, frame: Rect| CgWindowBasics {
            wid,
            pid,
            level,
            frame,
        };
        let frame = Rect::new(0.0, 0.0, 400.0, 300.0);
        let own = 7;
        let pids = listed_pids(
            [
                window(10, 300, 0, frame),
                window(11, 200, 0, frame),
                window(12, 300, 0, frame),
                // Filtered: no window id, our own window, a non-zero level and
                // an empty frame.
                window(0, 400, 0, frame),
                window(13, own, 0, frame),
                window(14, 500, 25, frame),
                window(15, 600, 0, Rect::new(0.0, 0.0, 0.0, 300.0)),
            ],
            own,
        );
        assert_eq!(pids, vec![200, 300]);
        assert!(listed_pids([], own).is_empty());
    }

    #[test]
    fn all_levels_filter_keeps_raised_windows_and_drops_unreal_ones() {
        let window = |wid: u32, pid: libc::pid_t, level: i32, frame: Rect| CgWindowBasics {
            wid,
            pid,
            level,
            frame,
        };
        let frame = Rect::new(0.0, 0.0, 1440.0, 32.0);
        let own = 7;
        // A status-level panel is a foreign window but not a listed one.
        let panel = window(10, 300, 25, frame);
        assert!(panel.is_foreign_window(own));
        assert!(!panel.is_listed(own));
        assert!(window(11, 300, 0, frame).is_foreign_window(own));
        assert!(window(11, 300, 0, frame).is_listed(own));
        // No window id, our own window and an empty frame are never listed.
        assert!(!window(0, 300, 25, frame).is_foreign_window(own));
        assert!(!window(12, own, 25, frame).is_foreign_window(own));
        assert!(!window(13, 300, 25, Rect::new(0.0, 0.0, 1440.0, 0.0)).is_foreign_window(own));
    }

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
        let shrink = Size::new(1.0, 1.0);
        let grow = Size::new(928.0, 728.0);

        let free =
            SizeConstraints::from_probes(original, shrink, shrink, Some(grow), Some(grow), None);
        assert_eq!(free.width, AxisConstraint::Free);
        assert_eq!(free.height, AxisConstraint::Free);
        assert_eq!(free.min, None);
        assert_eq!(free.max, None);

        // A real minimum: the shrink was clamped, the grow was applied.
        let width_min = SizeConstraints::from_probes(
            original,
            shrink,
            Size::new(642.0, 1.0),
            Some(grow),
            Some(grow),
            None,
        );
        assert_eq!(width_min.width, AxisConstraint::Min { min: 642.0 });
        assert_eq!(width_min.height, AxisConstraint::Free);
        assert_eq!(width_min.min, Some(Size::new(642.0, 0.0)));
        assert_eq!(width_min.max, None);

        // A genuinely frozen dimension refuses both directions.
        let frozen = SizeConstraints::from_probes(
            Size::new(715.0, 600.0),
            shrink,
            Size::new(715.0, 1.0),
            Some(grow),
            Some(Size::new(715.0, 728.0)),
            None,
        );
        assert_eq!(frozen.width, AxisConstraint::Frozen { value: 715.0 });
        assert_eq!(frozen.min, Some(Size::new(715.0, 0.0)));
        assert_eq!(frozen.max, Some(Size::new(715.0, f64::MAX)));
    }

    #[test]
    fn minimum_equal_to_natural_size_is_min_not_frozen() {
        // The VM's Notes case: 642px is both its natural width and its minimum, but it can
        // still grow.  Reporting Frozen here was the bug.
        let constraints = SizeConstraints::from_probes(
            Size::new(642.0, 600.0),
            Size::new(1.0, 1.0),
            Size::new(642.0, 1.0),
            Some(Size::new(770.0, 728.0)),
            Some(Size::new(770.0, 728.0)),
            None,
        );
        assert_eq!(constraints.width, AxisConstraint::Min { min: 642.0 });
        assert_eq!(constraints.min, Some(Size::new(642.0, 0.0)));
        assert_eq!(constraints.max, None);
    }

    #[test]
    fn grow_clamp_is_reported_as_a_maximum() {
        // Grow-only maximum: the shrink was free, the grow stopped at 850, refusing more
        // than half of the step.
        let width_max = SizeConstraints::from_probes(
            Size::new(800.0, 600.0),
            Size::new(1.0, 1.0),
            Size::new(1.0, 1.0),
            Some(Size::new(928.0, 728.0)),
            Some(Size::new(850.0, 728.0)),
            None,
        );
        assert_eq!(width_max.width, AxisConstraint::Free);
        assert_eq!(width_max.height, AxisConstraint::Free);
        assert_eq!(width_max.min, None);
        assert_eq!(width_max.max, Some(Size::new(850.0, f64::MAX)));

        // A full refusal of the grow (the shrink still worked) is a maximum at the
        // current size.
        let refused = SizeConstraints::from_probes(
            Size::new(800.0, 600.0),
            Size::new(1.0, 1.0),
            Size::new(1.0, 1.0),
            Some(Size::new(928.0, 728.0)),
            Some(Size::new(800.0, 728.0)),
            None,
        );
        assert_eq!(refused.width, AxisConstraint::Free);
        assert_eq!(refused.max, Some(Size::new(800.0, f64::MAX)));

        // A Min and a max on the same axis.
        let range = SizeConstraints::from_probes(
            Size::new(642.0, 600.0),
            Size::new(1.0, 1.0),
            Size::new(642.0, 1.0),
            Some(Size::new(770.0, 728.0)),
            Some(Size::new(700.0, 728.0)),
            None,
        );
        assert_eq!(range.width, AxisConstraint::Min { min: 642.0 });
        assert_eq!(range.max, Some(Size::new(700.0, f64::MAX)));
    }

    #[test]
    fn grow_snapping_short_of_the_request_is_not_a_maximum() {
        // Terminal at 896×504 with 7×14px cells: the +128 grow comes back as +126 wide and
        // +126 high.  Recording that as a maximum was the bug.
        let terminal = SizeConstraints::from_probes(
            Size::new(896.0, 504.0),
            Size::new(1.0, 1.0),
            Size::new(140.0, 70.0),
            Some(Size::new(1024.0, 632.0)),
            Some(Size::new(1022.0, 630.0)),
            None,
        );
        assert_eq!(terminal.width, AxisConstraint::Min { min: 140.0 });
        assert_eq!(terminal.height, AxisConstraint::Min { min: 70.0 });
        assert_eq!(terminal.min, Some(Size::new(140.0, 70.0)));
        assert_eq!(terminal.max, None, "a grid snap is not a maximum");

        // The threshold: a shortfall of exactly half the step is still a snap, more is not.
        let probe = |applied_width: f64| {
            SizeConstraints::from_probes(
                Size::new(800.0, 600.0),
                Size::new(1.0, 1.0),
                Size::new(1.0, 1.0),
                Some(Size::new(928.0, 728.0)),
                Some(Size::new(applied_width, 728.0)),
                None,
            )
            .max
        };
        assert_eq!(probe(928.0 - GROW_SNAP_LIMIT), None);
        assert_eq!(
            probe(927.0 - GROW_SNAP_LIMIT),
            Some(Size::new(927.0 - GROW_SNAP_LIMIT, f64::MAX))
        );

        // A grow the display cap shortened to less than the limit cannot tell a maximum from
        // a snap, even when it is refused outright; the daemon's repeated clamps decide.
        let capped = SizeConstraints::from_probes(
            Size::new(800.0, 600.0),
            Size::new(1.0, 1.0),
            Size::new(1.0, 1.0),
            Some(Size::new(850.0, 650.0)),
            Some(Size::new(800.0, 600.0)),
            None,
        );
        assert_eq!(capped.width, AxisConstraint::Free);
        assert_eq!(capped.max, None);
    }

    #[test]
    fn grow_snapping_upward_is_not_a_maximum() {
        let constraints = SizeConstraints::from_probes(
            Size::new(800.0, 600.0),
            Size::new(1.0, 1.0),
            Size::new(1.0, 1.0),
            Some(Size::new(928.0, 728.0)),
            Some(Size::new(930.0, 730.0)),
            None,
        );
        assert_eq!(constraints.width, AxisConstraint::Free);
        assert_eq!(constraints.max, None);
    }

    #[test]
    fn declared_maximum_is_used_without_growing_the_window() {
        let original = Size::new(800.0, 600.0);
        let shrink = Size::new(1.0, 1.0);
        let declared = Size::new(4000.0, 3000.0);
        let constraints =
            SizeConstraints::from_probes(original, shrink, shrink, None, None, Some(declared));
        assert_eq!(constraints.max, Some(declared));
        assert_eq!(constraints.min, None);

        // A frozen axis is at most its frozen value, even if AXMaxSize is larger.
        let frozen = SizeConstraints::from_probes(
            original,
            shrink,
            Size::new(800.0, 1.0),
            None,
            None,
            Some(Size::new(4000.0, 3000.0)),
        );
        assert_eq!(frozen.max, Some(Size::new(800.0, 3000.0)));
        assert_eq!(frozen.width, AxisConstraint::Frozen { value: 800.0 });
    }

    #[test]
    fn shrink_only_probe_degrades_to_frozen() {
        // Without a display we cannot grow, so a refused shrink is the conservative answer.
        let constraints = SizeConstraints::from_probes(
            Size::new(642.0, 600.0),
            Size::new(1.0, 1.0),
            Size::new(642.0, 1.0),
            None,
            None,
            None,
        );
        assert_eq!(constraints.width, AxisConstraint::Frozen { value: 642.0 });
        assert_eq!(constraints.max, Some(Size::new(642.0, f64::MAX)));
    }

    #[test]
    fn probe_target_equal_to_current_size_reports_no_constraint() {
        let original = Size::new(1.0, 1.0);
        let constraints =
            SizeConstraints::from_probes(original, original, original, None, None, None);
        assert!(constraints.width.is_free());
        assert!(constraints.height.is_free());
        assert_eq!(constraints.min, None);
    }

    #[test]
    fn probe_target_helpers_respect_the_display_cap() {
        assert_eq!(shrink_target(Size::new(800.0, 600.0)), Size::new(1.0, 1.0));
        assert_eq!(shrink_target(Size::new(0.5, 0.5)), Size::new(0.5, 0.5));
        assert_eq!(
            grow_target(Size::new(800.0, 600.0), Some(Size::new(2000.0, 2000.0))),
            Some(Size::new(928.0, 728.0))
        );
        assert_eq!(
            grow_target(Size::new(800.0, 600.0), Some(Size::new(850.0, 700.0))),
            Some(Size::new(850.0, 700.0))
        );
        assert_eq!(
            grow_target(Size::new(800.0, 600.0), Some(Size::new(700.0, 500.0))),
            None
        );
        assert_eq!(grow_target(Size::new(800.0, 600.0), None), None);
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
