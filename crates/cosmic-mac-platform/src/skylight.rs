//! All private SkyLight FFI lives here.
//!
//! SkyLight (`/System/Library/PrivateFrameworks/SkyLight.framework`) is the
//! WindowServer client library. It is private but stable enough that yabai,
//! JankyBorders, rift and glide all use it with SIP enabled. We load it with
//! `dlopen` instead of linking it so that:
//!
//! * the crate builds and its unit tests run even if the framework moves;
//! * a missing symbol produces a clear [`PlatformError::SkyLightUnavailable`]
//!   instead of a link error;
//! * the same binary can be inspected with `otool -L` without a private
//!   framework dependency (useful for code review and for TCC).
//!
//! Every prototype below was cross-checked against yabai's
//! `src/misc/extern.h`, JankyBorders' `src/misc/extern.h` and rift's
//! `src/sys/skylight.rs` (2026-10). See `docs/SKYLIGHT.md` for the provenance of
//! each call and for the macOS 15/26 message-ID notes.
//!
//! Safety: all wrappers are `unsafe` because they take raw pointers or window
//! IDs owned by another process. The safe API in [`crate::window`],
//! [`crate::overlay`] and [`crate::observers`] is the intended entry point.

#![allow(dead_code)] // The FFI surface is declared completely even before every call site exists.

use std::ffi::{CString, c_int, c_void};
use std::sync::OnceLock;

use objc2_core_foundation::{CGPoint, CGRect};

use crate::error::{PlatformError, Result};

/// `CGSConnectionID` of the calling process.
pub type ConnectionId = c_int;

/// Window server window ID (`CGWindowID`).
pub type WindowId = u32;

/// Managed Space ID.
pub type SpaceId = u64;

/// SkyLight path on macOS 10.6 through 26.
const SKYLIGHT_PATH: &str = "/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";

fn handle() -> Result<*mut c_void> {
    static HANDLE: OnceLock<usize> = OnceLock::new();
    let handle = HANDLE.get_or_init(|| unsafe {
        let path = CString::new(SKYLIGHT_PATH).expect("static path");
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        handle as usize
    });
    if *handle == 0 {
        Err(PlatformError::SkyLightUnavailable(format!(
            "dlopen({SKYLIGHT_PATH}) failed"
        )))
    } else {
        Ok(*handle as *mut c_void)
    }
}

/// Resolve a symbol lazily. `dlsym` is a hash lookup; calls happen at window
/// event rates, so caching is unnecessary and would only add global state.
fn resolve<T: Copy>(name: &'static str) -> Result<T> {
    let handle = handle()?;
    let cname = CString::new(name).expect("symbol names are NUL-free");
    let symbol = unsafe { libc::dlsym(handle, cname.as_ptr()) };
    if symbol.is_null() {
        return Err(PlatformError::SkyLightUnavailable(format!(
            "symbol {name} not found in SkyLight"
        )));
    }
    debug_assert_eq!(
        std::mem::size_of::<T>(),
        std::mem::size_of::<*mut c_void>(),
        "function pointers are pointer sized"
    );
    // SAFETY: `T` is always a function-pointer type in this module and the
    // size assertion above guarantees the representation matches.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&symbol) })
}

macro_rules! sl {
    ($(fn $name:ident($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty;)*) => {$(
        #[allow(clippy::missing_safety_doc)]
        #[allow(non_snake_case)]
        #[allow(clippy::too_many_arguments)]
        pub(crate) unsafe fn $name($($arg: $ty),*) -> Result<$ret> {
            type F = unsafe extern "C" fn($($ty),*) -> $ret;
            let f: F = resolve(stringify!($name))?;
            // SAFETY: the caller upholds the contract of the private function.
            Ok(unsafe { f($($arg),*) })
        }
    )*};
}

sl! {
    fn SLSMainConnectionID() -> ConnectionId;
    fn SLSNewConnection(zero: c_int, cid: *mut ConnectionId) -> i32;
    fn SLSReleaseConnection(cid: ConnectionId) -> i32;
    fn SLSGetWindowBounds(cid: ConnectionId, wid: WindowId, frame: *mut CGRect) -> i32;
    fn SLSGetWindowLevel(cid: ConnectionId, wid: WindowId, level: *mut i32) -> i32;
    fn SLSGetWindowSubLevel(cid: ConnectionId, wid: WindowId) -> i32;
    fn SLSGetWindowAlpha(cid: ConnectionId, wid: WindowId, alpha: *mut f32) -> i32;
    fn SLSGetWindowOwner(cid: ConnectionId, wid: WindowId, owner: *mut ConnectionId) -> i32;
    fn SLSConnectionGetPID(cid: ConnectionId, pid: *mut libc::pid_t) -> i32;
    fn SLSOrderWindow(cid: ConnectionId, wid: WindowId, order: c_int, relative_to: WindowId) -> i32;
    fn SLSMoveWindow(cid: ConnectionId, wid: WindowId, point: *mut CGPoint) -> i32;
    fn SLSWindowIsOrderedIn(cid: ConnectionId, wid: WindowId, ordered: *mut u8) -> i32;
    fn SLSDisableUpdate(cid: ConnectionId) -> i32;
    fn SLSReenableUpdate(cid: ConnectionId) -> i32;

    fn SLSRequestNotificationsForWindows(
        cid: ConnectionId,
        windows: *const WindowId,
        count: c_int,
    ) -> i32;

    fn SLSNewWindow(
        cid: ConnectionId,
        store_type: c_int,
        x: f32,
        y: f32,
        region: *mut c_void,
        wid: *mut WindowId,
    ) -> i32;

    fn SLSNewWindowWithOpaqueShapeAndContext(
        cid: ConnectionId,
        store_type: c_int,
        region: *mut c_void,
        opaque_shape: *mut c_void,
        options: c_int,
        tags: *mut u64,
        x: f32,
        y: f32,
        tag_size: c_int,
        wid: *mut WindowId,
        context: *mut c_void,
    ) -> i32;

    fn SLSReleaseWindow(cid: ConnectionId, wid: WindowId) -> i32;
    fn SLSSetWindowTags(cid: ConnectionId, wid: WindowId, tags: *mut u64, tag_size: c_int) -> i32;
    fn SLSClearWindowTags(cid: ConnectionId, wid: WindowId, tags: *mut u64, tag_size: c_int) -> i32;
    fn SLSSetWindowShape(
        cid: ConnectionId,
        wid: WindowId,
        x_offset: f32,
        y_offset: f32,
        shape: *mut c_void,
    ) -> i32;
    fn SLSSetWindowResolution(cid: ConnectionId, wid: WindowId, resolution: f64) -> i32;
    fn SLSSetWindowOpacity(cid: ConnectionId, wid: WindowId, opaque: bool) -> i32;
    fn SLSSetWindowAlpha(cid: ConnectionId, wid: WindowId, alpha: f32) -> i32;
    fn SLSSetWindowLevel(cid: ConnectionId, wid: WindowId, level: c_int) -> i32;
    fn SLSSetWindowSubLevel(cid: ConnectionId, wid: WindowId, level: c_int) -> i32;

    fn SLSTransactionCreate(cid: ConnectionId) -> *mut c_void;
    fn SLSTransactionCommit(transaction: *mut c_void, synchronous: c_int) -> i32;
    fn SLSTransactionOrderWindow(
        transaction: *mut c_void,
        wid: WindowId,
        order: c_int,
        relative_to: WindowId,
    ) -> i32;
    fn SLSTransactionSetWindowLevel(
        transaction: *mut c_void,
        wid: WindowId,
        level: c_int,
    ) -> i32;
    fn SLSTransactionSetWindowSubLevel(
        transaction: *mut c_void,
        wid: WindowId,
        level: c_int,
    ) -> i32;
    fn SLSTransactionMoveWindowWithGroup(
        transaction: *mut c_void,
        wid: WindowId,
        point: CGPoint,
    ) -> i32;

    fn SLSGetEventPort(cid: ConnectionId, port: *mut u32) -> i32;
    fn SLEventCreateNextEvent(cid: ConnectionId) -> *mut c_void;

    fn SLSWindowFreezeWithOptions(
        cid: ConnectionId,
        wid: WindowId,
        options: *mut c_void,
    ) -> i32;
    fn SLSWindowThaw(cid: ConnectionId, wid: WindowId) -> i32;

    fn SLWindowContextCreate(
        cid: ConnectionId,
        wid: WindowId,
        options: *mut c_void,
    ) -> *mut c_void;
    fn SLSFlushWindowContentRegion(cid: ConnectionId, wid: WindowId, region: *mut c_void) -> i32;
    fn SLSWindowSetShadowProperties(wid: WindowId, properties: *mut c_void) -> i32;

    fn CGSNewRegionWithRect(rect: *const CGRect, region: *mut *mut c_void) -> i32;
    fn CGRegionCreateEmptyRegion() -> *mut c_void;

    fn SLSCopyWindowsWithOptionsAndTags(
        cid: ConnectionId,
        owner: u32,
        spaces: *mut c_void,
        options: u32,
        set_tags: *mut u64,
        clear_tags: *mut u64,
    ) -> *mut c_void;
    fn SLSCopySpacesForWindows(
        cid: ConnectionId,
        selector: c_int,
        windows: *mut c_void,
    ) -> *mut c_void;
    // NOTE: yabai declares this as returning void. Treating the (absent)
    // return value as a CGError yields garbage error codes, which cost an hour
    // during Spike C.
    fn SLSMoveWindowsToManagedSpace(
        cid: ConnectionId,
        windows: *mut c_void,
        sid: SpaceId,
    ) -> ();
    fn SLSSpaceGetType(cid: ConnectionId, sid: SpaceId) -> c_int;
    fn SLSGetActiveSpace(cid: ConnectionId) -> SpaceId;

    fn _SLPSSetFrontProcessWithOptions(
        psn: *const ProcessSerialNumber,
        wid: WindowId,
        mode: u32,
    ) -> i32;
    fn _SLPSGetFrontProcess(psn: *mut ProcessSerialNumber) -> i32;
    fn SLPSPostEventRecordTo(psn: *const ProcessSerialNumber, bytes: *const u8) -> i32;
    fn SLSGetConnectionIDForPSN(
        cid: ConnectionId,
        psn: *const ProcessSerialNumber,
        psn_cid: *mut ConnectionId,
    ) -> i32;
}

/// `SLSRegisterConnectionNotifyProc` callback.
pub type ConnectionNotifyCallback =
    unsafe extern "C" fn(u32, *mut c_void, usize, *mut c_void, ConnectionId);

/// Register a per-connection notification handler.
///
/// The callback runs on the main run loop. There is no unregister API exposed by
/// SkyLight; register once per process and dispatch through a global handler.
pub(crate) unsafe fn register_connection_notify_proc(
    cid: ConnectionId,
    callback: ConnectionNotifyCallback,
    event: u32,
    context: *mut c_void,
) -> Result<i32> {
    type F = unsafe extern "C" fn(ConnectionId, ConnectionNotifyCallback, u32, *mut c_void) -> i32;
    let f: F = resolve("SLSRegisterConnectionNotifyProc")?;
    // SAFETY: caller keeps `context` alive for the process lifetime.
    Ok(unsafe { f(cid, callback, event, context) })
}

/// `SLSRegisterNotifyProc` (no connection argument). JankyBorders uses this
/// variant for its border window notifications.
#[allow(unused)]
pub(crate) unsafe fn register_notify_proc(
    callback: ConnectionNotifyCallback,
    event: u32,
    context: *mut c_void,
) -> Result<i32> {
    type F = unsafe extern "C" fn(ConnectionNotifyCallback, u32, *mut c_void) -> i32;
    let f: F = resolve("SLSRegisterNotifyProc")?;
    // SAFETY: caller keeps `context` alive for the process lifetime.
    Ok(unsafe { f(callback, event, context) })
}

/// Carbon `ProcessSerialNumber`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProcessSerialNumber {
    /// High 32 bits.
    pub high: u32,
    /// Low 32 bits.
    pub low: u32,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Carbon helper: resolve a Unix PID to a `ProcessSerialNumber`.
    fn GetProcessForPID(pid: libc::pid_t, psn: *mut ProcessSerialNumber) -> i32;
}

/// Resolve the Carbon `ProcessSerialNumber` for a PID.
pub fn process_serial_number(pid: libc::pid_t) -> Result<ProcessSerialNumber> {
    let mut psn = ProcessSerialNumber::default();
    // SAFETY: `psn` is a valid out-pointer.
    let err = unsafe { GetProcessForPID(pid, &mut psn) };
    if err == 0 {
        Ok(psn)
    } else {
        Err(PlatformError::Os {
            context: "GetProcessForPID",
            message: format!("OSStatus {err} for pid {pid}"),
        })
    }
}

/// Start the SkyLight event pump on a dedicated thread.
///
/// `SLSRegisterNotifyProc` callbacks are only invoked while the connection's
/// event port is being drained. JankyBorders drains it from a CFMachPort run
/// loop source; a blocking thread is simpler and keeps the pump off the main
/// run loop. The thread lives for the process lifetime.
pub(crate) fn start_event_pump(cid: ConnectionId) -> Result<()> {
    use std::sync::{Once, OnceLock};
    static STARTED: Once = Once::new();
    static RESULT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

    STARTED.call_once(|| {
        // SAFETY: valid connection and out-pointer.
        let mut port: u32 = 0;
        let result = unsafe {
            match SLSGetEventPort(cid, &mut port) {
                Ok(0) if port != 0 => Ok(()),
                Ok(code) => Err(format!("SLSGetEventPort returned {code} (port {port})")),
                Err(err) => Err(err.to_string()),
            }
        };
        let result = result.and_then(|()| {
            std::thread::Builder::new()
                .name("skylight-event-pump".into())
                .spawn(move || {
                    loop {
                        // SAFETY: valid connection. Returns null when the queue
                        // is empty; the event is +1 when non-null.
                        let event = match unsafe { SLEventCreateNextEvent(cid) } {
                            Ok(event) => event,
                            Err(_) => {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                                continue;
                            }
                        };
                        if event.is_null() {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        } else {
                            // SAFETY: +1 CGEventRef.
                            unsafe { cf_release(event) };
                        }
                    }
                })
                .map(|_| ())
                .map_err(|err| format!("cannot spawn event pump: {err}"))
        });
        let _ = RESULT.set(result);
    });

    match RESULT.get() {
        Some(Ok(())) => Ok(()),
        Some(Err(message)) => Err(PlatformError::SkyLightUnavailable(message.clone())),
        None => Err(PlatformError::Other(
            "SkyLight event pump initialization failed".into(),
        )),
    }
}

/// The calling process' connection to the WindowServer.
pub fn main_connection() -> Result<ConnectionId> {
    // SAFETY: no arguments.
    let cid = unsafe { SLSMainConnectionID()? };
    if cid == 0 {
        return Err(PlatformError::SkyLightUnavailable(
            "SLSMainConnectionID returned 0 (no GUI session?)".into(),
        ));
    }
    Ok(cid)
}

/// `kCGSOrderAbove`.
pub(crate) const ORDER_ABOVE: c_int = 1;
/// `kCGSOrderBelow`.
pub(crate) const ORDER_BELOW: c_int = -1;

/// `kCGBackingStoreBuffered`.
pub(crate) const BACKING_STORE_BUFFERED: c_int = 2;

/// Tags used by JankyBorders for an overlay: floating + ignore-for-events.
pub(crate) const OVERLAY_TAGS: u64 = (1u64 << 1) | (1u64 << 9);

/// Bit tag meaning "the window is miniaturized" (`SLSWindowTags::MINIATURIZED`).
pub(crate) const WINDOW_TAG_MINIATURIZED: u64 = 1u64 << 60;

/// Event IDs delivered through `SLSRegisterConnectionNotifyProc`.
///
/// Names and values are from rift's `KnownCGSEvent` (itself derived from yabai
/// and `CGSInternal`). Only the subset this project needs is listed.
pub mod events {
    /// A window disappeared (closed or destroyed).
    pub const WINDOW_CLOSED: u32 = 804;
    /// A window's display assignment changed.
    pub const WINDOW_DISPLAY_CHANGED: u32 = 805;
    /// A window moved.
    pub const WINDOW_MOVED: u32 = 806;
    /// A window was resized.
    pub const WINDOW_RESIZED: u32 = 807;
    /// A window was reordered in the z stack.
    pub const WINDOW_REORDERED: u32 = 808;
    /// A window's level changed.
    pub const WINDOW_LEVEL_CHANGED: u32 = 811;
    /// A window was unhidden.
    pub const WINDOW_UNHIDDEN: u32 = 815;
    /// A window was hidden.
    pub const WINDOW_HIDDEN: u32 = 816;
    /// A window's title changed (Sequoia+).
    pub const WINDOW_TITLE_CHANGED: u32 = 1322;
    /// A window was added to a Space.
    pub const SPACE_WINDOW_CREATED: u32 = 1325;
    /// A window was removed from a Space.
    pub const SPACE_WINDOW_DESTROYED: u32 = 1326;
    /// The frontmost application changed.
    pub const FRONTMOST_APPLICATION_CHANGED: u32 = 1508;
}

/// Read the window server bounds of `wid`.
pub fn window_bounds(wid: WindowId) -> Result<CGRect> {
    let cid = main_connection()?;
    let mut frame = CGRect::default();
    // SAFETY: valid connection and out-pointer.
    crate::error::cg_ok("SLSGetWindowBounds", unsafe {
        SLSGetWindowBounds(cid, wid, &mut frame)?
    })?;
    Ok(frame)
}

/// Read the window level (layer) of `wid`.
pub fn window_level(wid: WindowId) -> Result<i32> {
    let cid = main_connection()?;
    let mut level = 0i32;
    // SAFETY: valid connection and out-pointer.
    crate::error::cg_ok("SLSGetWindowLevel", unsafe {
        SLSGetWindowLevel(cid, wid, &mut level)?
    })?;
    Ok(level)
}

/// Read the window sub-level of `wid` (used when copying an overlay's stacking).
pub fn window_sub_level(wid: WindowId) -> Result<i32> {
    let cid = main_connection()?;
    // SAFETY: valid connection.
    Ok(unsafe { SLSGetWindowSubLevel(cid, wid)? })
}

/// PID owning `wid`, resolved through the window server (no Accessibility needed).
pub fn window_owner_pid(wid: WindowId) -> Result<libc::pid_t> {
    let cid = main_connection()?;
    let mut owner_cid: ConnectionId = 0;
    // SAFETY: valid connection and out-pointer.
    crate::error::cg_ok("SLSGetWindowOwner", unsafe {
        SLSGetWindowOwner(cid, wid, &mut owner_cid)?
    })?;
    let mut pid: libc::pid_t = 0;
    // SAFETY: valid connection id and out-pointer.
    crate::error::cg_ok("SLSConnectionGetPID", unsafe {
        SLSConnectionGetPID(owner_cid, &mut pid)?
    })?;
    Ok(pid)
}

/// Whether the window is currently ordered in (visible to the compositor).
pub fn window_is_ordered_in(wid: WindowId) -> Result<bool> {
    let cid = main_connection()?;
    let mut ordered: u8 = 0;
    // SAFETY: valid connection and out-pointer.
    crate::error::cg_ok("SLSWindowIsOrderedIn", unsafe {
        SLSWindowIsOrderedIn(cid, wid, &mut ordered)?
    })?;
    Ok(ordered != 0)
}

/// `SLSSpaceGetType` value of a native full-screen Space.
pub const SPACE_TYPE_FULLSCREEN: i32 = 4;

/// The Space currently shown on the active display.
pub fn active_space() -> Result<SpaceId> {
    let cid = main_connection()?;
    // SAFETY: valid connection.
    let sid = unsafe { SLSGetActiveSpace(cid)? };
    if sid == 0 {
        return Err(PlatformError::Other("SLSGetActiveSpace returned 0".into()));
    }
    Ok(sid)
}

/// The type of a managed Space (`0` a user desktop, [`SPACE_TYPE_FULLSCREEN`]
/// a native full-screen Space).
pub fn space_type(sid: SpaceId) -> Result<i32> {
    let cid = main_connection()?;
    // SAFETY: valid connection; unknown ids yield a type, not a crash.
    Ok(unsafe { SLSSpaceGetType(cid, sid)? })
}

/// The managed Spaces `wid` is on (`SLSCopySpacesForWindows`, selector `0x7`:
/// every Space, not only the visible ones).
///
/// Empty for a window the window server no longer knows, and for a window
/// that is not on any managed Space.
pub fn window_spaces(wid: WindowId) -> Result<Vec<SpaceId>> {
    use objc2_core_foundation::{CFArray, CFNumber, CFRetained, CFType};

    let cid = main_connection()?;
    let wid_number = CFNumber::new_i64(i64::from(wid));
    let windows: CFRetained<CFArray<CFType>> = CFArray::from_objects(&[&*wid_number]);
    // SAFETY: valid connection and a CFArray of CFNumbers.
    let spaces =
        unsafe { SLSCopySpacesForWindows(cid, 0x7, CFRetained::as_ptr(&windows).as_ptr().cast())? };
    if spaces.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: a +1 CFArray of CFNumbers; released after reading.
    let array: &CFArray<CFType> = unsafe { (&*(spaces as *const CFArray)).cast_unchecked() };
    let ids = (0..array.len())
        .filter_map(|index| array.get(index))
        .filter_map(|value| value.downcast::<CFNumber>().ok())
        .filter_map(|value| value.as_i64())
        .map(|value| value as SpaceId)
        .collect();
    // SAFETY: we own the +1 reference.
    unsafe { cf_release(spaces) };
    Ok(ids)
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// Release a +1 CoreFoundation reference (used for region/transaction objects
/// returned by SkyLight).
///
/// # Safety
/// `ptr` must be a +1 CF reference or null.
pub(crate) unsafe fn cf_release(ptr: *const c_void) {
    if !ptr.is_null() {
        // SAFETY: caller owns a +1 reference.
        unsafe { CFRelease(ptr) };
    }
}
