//! Window, application and display observers.
//!
//! Two independent event sources are joined here:
//!
//! * **SkyLight** connection notifications (`SLSRegisterConnectionNotifyProc`)
//!   for move/resize/order/hide/title/space/front-app events. These work
//!   without Accessibility, which makes them the primary geometry signal.
//! * **AX observers** (one `AXObserver` per application) for titles, focus,
//!   window creation/destruction and app activation. These need Accessibility.
//!
//! Both callbacks are delivered on the run loop that owns them (normally the
//! main thread's). [`PlatformObserver`] installs the callbacks and forwards a
//! unified [`PlatformEvent`] to the caller's handler.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use objc2_core_foundation::CFString;

use crate::ax::{ATTR_FOCUSED_WINDOW, ATTR_WINDOWS, AxElement, AxObserver, AxObserverRef};
use crate::error::Result;
use crate::geometry::Rect;
use crate::skylight::{self, events as sls_events};
use crate::window;

/// A platform-level event.
#[derive(Debug, Clone, PartialEq)]
pub enum PlatformEvent {
    /// A window appeared.
    WindowCreated {
        /// Window ID.
        wid: u32,
        /// Owning process.
        pid: libc::pid_t,
    },
    /// A window disappeared.
    WindowDestroyed {
        /// Window ID.
        wid: u32,
    },
    /// A window moved.
    WindowMoved {
        /// Window ID.
        wid: u32,
        /// New frame (top-left global).
        frame: Rect,
    },
    /// A window was resized.
    WindowResized {
        /// Window ID.
        wid: u32,
        /// New frame (top-left global).
        frame: Rect,
    },
    /// A window changed position in the z stack.
    WindowOrdered {
        /// Window ID.
        wid: u32,
    },
    /// A window's level changed.
    WindowLevelChanged {
        /// Window ID.
        wid: u32,
    },
    /// A window's title changed.
    WindowTitleChanged {
        /// Window ID.
        wid: u32,
        /// New title (requires Accessibility).
        title: Option<String>,
    },
    /// A window was hidden, perhaps minimized.
    ///
    /// Comes from the SkyLight window-hidden notification, whose exact
    /// semantics are unverified (it may also fire when a Space change orders
    /// windows out).  Consumers should re-read the window's state instead of
    /// assuming it was minimized.
    WindowHidden {
        /// Window ID.
        wid: u32,
    },
    /// A window was shown, perhaps deminimized.
    ///
    /// From the SkyLight window-unhidden notification; the same caveat as
    /// [`PlatformEvent::WindowHidden`] applies.
    WindowShown {
        /// Window ID.
        wid: u32,
    },
    /// A window was minimized (`AXWindowMiniaturized`).
    ///
    /// Authoritative, unlike [`PlatformEvent::WindowHidden`], but early: the
    /// notification fires when the minimize animation starts, and the
    /// window's `AXMinimized` attribute keeps reading false for about half a
    /// second after it.
    WindowMinimized {
        /// Window ID.
        wid: u32,
    },
    /// A minimized window was restored (`AXWindowDeminiaturized`).  The same
    /// caveat as [`PlatformEvent::WindowMinimized`] applies to
    /// `AXMinimized`.
    WindowDeminimized {
        /// Window ID.
        wid: u32,
    },
    /// An application was hidden (Cmd+H, `AXApplicationHidden`): all of its
    /// windows are off screen.
    AppHidden {
        /// The hidden application's PID.
        pid: libc::pid_t,
    },
    /// A hidden application was shown again (`AXApplicationShown`).
    AppShown {
        /// The application's PID.
        pid: libc::pid_t,
    },
    /// Focus moved.
    FocusChanged {
        /// Focused window, when known.
        wid: Option<u32>,
        /// Owning process, when known.
        pid: Option<libc::pid_t>,
    },
    /// The frontmost application changed.
    AppActivated {
        /// New frontmost PID, when the payload could be decoded.
        pid: Option<libc::pid_t>,
    },
    /// A display was added, removed or reconfigured.
    DisplayChanged,
    /// An event we do not decode yet, kept for diagnostics.
    UnhandledSls {
        /// Raw SkyLight event ID.
        event: u32,
        /// Window ID if the payload started with one.
        wid: Option<u32>,
        /// Payload length.
        len: usize,
    },
}

/// Event handler type.
pub type EventHandler = Arc<dyn Fn(PlatformEvent) + Send + Sync>;

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let slice = bytes.get(offset..end)?;
    Some(u32::from_ne_bytes(slice.try_into().ok()?))
}

static SLS_HANDLER: OnceLock<Mutex<Option<EventHandler>>> = OnceLock::new();
static AX_HANDLER: OnceLock<Mutex<Option<EventHandler>>> = OnceLock::new();

fn sls_handler() -> Option<EventHandler> {
    SLS_HANDLER
        .get()
        .and_then(|slot| slot.lock().ok())
        .and_then(|slot| slot.clone())
}

fn ax_handler() -> Option<EventHandler> {
    AX_HANDLER
        .get()
        .and_then(|slot| slot.lock().ok())
        .and_then(|slot| slot.clone())
}

/// Which geometry notification to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeometryKind {
    Moved,
    Resized,
}

/// Build a move/resize event from a bounds read.
///
/// A failed read (typically a window that is closing) yields no event: a
/// fabricated `Rect::default()` would look like a user move to the origin and
/// provoke a snap-back attempt on a window that is going away.
fn geometry_event(wid: u32, frame: Option<Rect>, kind: GeometryKind) -> Option<PlatformEvent> {
    let Some(frame) = frame else {
        log::debug!("window {wid}: bounds unreadable; dropping {kind:?} event");
        return None;
    };
    Some(match kind {
        GeometryKind::Moved => PlatformEvent::WindowMoved { wid, frame },
        GeometryKind::Resized => PlatformEvent::WindowResized { wid, frame },
    })
}

fn sls_bounds(wid: u32) -> Option<Rect> {
    skylight::window_bounds(wid).ok().map(Rect::from_cg)
}

fn ax_frame(element: &AxElement) -> Option<Rect> {
    element.frame().ok().flatten()
}

unsafe extern "C" fn sls_callback(
    event: u32,
    data: *mut c_void,
    len: usize,
    context: *mut c_void,
    _cid: i32,
) {
    let Some(handler) = sls_handler() else {
        return;
    };
    // The context is the event ID we registered, leaked for the process
    // lifetime by `PlatformObserver::start_sls`.
    let registered = if context.is_null() {
        event
    } else {
        // SAFETY: context points to a leaked u32 created by start_sls.
        unsafe { *(context as *const u32) }
    };
    let _ = registered;

    let bytes: &[u8] = if data.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: SkyLight provides `len` readable bytes for the duration of
        // the callback.
        unsafe { std::slice::from_raw_parts(data as *const u8, len) }
    };

    let event = match event {
        sls_events::WINDOW_CLOSED => {
            read_u32(bytes, 0).map(|wid| PlatformEvent::WindowDestroyed { wid })
        }
        sls_events::WINDOW_MOVED => read_u32(bytes, 0)
            .and_then(|wid| geometry_event(wid, sls_bounds(wid), GeometryKind::Moved)),
        sls_events::WINDOW_RESIZED => read_u32(bytes, 0)
            .and_then(|wid| geometry_event(wid, sls_bounds(wid), GeometryKind::Resized)),
        sls_events::WINDOW_REORDERED => {
            read_u32(bytes, 0).map(|wid| PlatformEvent::WindowOrdered { wid })
        }
        sls_events::WINDOW_LEVEL_CHANGED => {
            read_u32(bytes, 0).map(|wid| PlatformEvent::WindowLevelChanged { wid })
        }
        sls_events::WINDOW_HIDDEN => {
            read_u32(bytes, 0).map(|wid| PlatformEvent::WindowHidden { wid })
        }
        sls_events::WINDOW_UNHIDDEN => {
            read_u32(bytes, 0).map(|wid| PlatformEvent::WindowShown { wid })
        }
        sls_events::WINDOW_TITLE_CHANGED => read_u32(bytes, 0).map(|wid| {
            let title = window::window_info(wid)
                .ok()
                .flatten()
                .and_then(|info| info.title);
            PlatformEvent::WindowTitleChanged { wid, title }
        }),
        // Space window events carry (u64 space, u32 window).
        sls_events::SPACE_WINDOW_CREATED => {
            read_u32(bytes, 8).map(|wid| PlatformEvent::WindowCreated {
                wid,
                pid: skylight::window_owner_pid(wid).unwrap_or(0),
            })
        }
        sls_events::SPACE_WINDOW_DESTROYED => {
            read_u32(bytes, 8).map(|wid| PlatformEvent::WindowDestroyed { wid })
        }
        sls_events::FRONTMOST_APPLICATION_CHANGED => {
            let pid = read_u32(bytes, 0).map(|value| value as libc::pid_t);
            Some(PlatformEvent::AppActivated { pid })
        }
        other => Some(PlatformEvent::UnhandledSls {
            event: other,
            wid: read_u32(bytes, 0),
            len,
        }),
    };

    if let Some(event) = event {
        // New windows do not emit move/resize notifications until they are
        // registered with SLSRequestNotificationsForWindows.
        if let PlatformEvent::WindowCreated { wid, .. } = &event
            && let Ok(cid) = skylight::main_connection()
        {
            // SAFETY: valid connection, one-element window slice.
            unsafe {
                let _ = skylight::SLSRequestNotificationsForWindows(cid, wid, 1);
            }
        }
        handler(event);
    }
}

/// AX observer callback context. Leaked for the observer's lifetime.
struct AxCallbackCtx {
    pid: libc::pid_t,
    observer: AxObserverRef,
}

/// AX notifications that are registered on the application element.
const APP_NOTIFICATIONS: &[&str] = &[
    "AXWindowCreated",
    "AXFocusedWindowChanged",
    "AXApplicationActivated",
    "AXApplicationHidden",
    "AXApplicationShown",
];

/// AX notifications that are registered on every window element.
const WINDOW_NOTIFICATIONS: &[&str] = &[
    "AXWindowMoved",
    "AXWindowResized",
    "AXTitleChanged",
    "AXWindowMiniaturized",
    "AXWindowDeminiaturized",
    "AXUIElementDestroyed",
];

/// Map an AX notification that carries no data beyond the element's window
/// id and the observed application's pid (minimize, destroy, app activation
/// and hiding).
///
/// Notifications that need further element reads (creation, geometry, title,
/// focus) are decoded in [`ax_callback`]; unknown names yield `None`.  A
/// window notification whose element has no window id yields `None` too.
fn ax_state_event(name: &str, wid: Option<u32>, pid: libc::pid_t) -> Option<PlatformEvent> {
    match name {
        "AXWindowMiniaturized" => wid.map(|wid| PlatformEvent::WindowMinimized { wid }),
        "AXWindowDeminiaturized" => wid.map(|wid| PlatformEvent::WindowDeminimized { wid }),
        "AXUIElementDestroyed" => wid.map(|wid| PlatformEvent::WindowDestroyed { wid }),
        "AXApplicationActivated" => Some(PlatformEvent::AppActivated { pid: Some(pid) }),
        "AXApplicationHidden" => Some(PlatformEvent::AppHidden { pid }),
        "AXApplicationShown" => Some(PlatformEvent::AppShown { pid }),
        _ => None,
    }
}

unsafe extern "C" fn ax_callback(
    _observer: AxObserverRef,
    element: crate::ax::AxElementRef,
    notification: *const c_void,
    refcon: *mut c_void,
) {
    let Some(handler) = ax_handler() else {
        return;
    };
    if refcon.is_null() {
        return;
    }
    // SAFETY: refcon points to an AxCallbackCtx owned by PlatformObserver.
    let context = unsafe { &*(refcon as *const AxCallbackCtx) };
    let name = if notification.is_null() {
        String::new()
    } else {
        // SAFETY: AX passes a borrowed CFString.
        let string = unsafe { &*(notification as *const CFString) };
        string.to_string()
    };
    // SAFETY: element is a borrowed AX element valid for this callback.
    let Some(element) = (unsafe { AxElement::from_borrowed(element) }) else {
        return;
    };
    let wid = element.window_id();

    let event = match name.as_str() {
        "AXWindowCreated" => {
            if let Some(wid) = wid {
                // New windows need their own move/resize/title notifications.
                for notification in WINDOW_NOTIFICATIONS {
                    let _ = crate::ax::add_notification_raw(
                        context.observer,
                        &element,
                        notification,
                        refcon,
                    );
                }
                Some(PlatformEvent::WindowCreated {
                    wid,
                    pid: context.pid,
                })
            } else {
                None
            }
        }
        "AXWindowMoved" => {
            wid.and_then(|wid| geometry_event(wid, ax_frame(&element), GeometryKind::Moved))
        }
        "AXWindowResized" => {
            wid.and_then(|wid| geometry_event(wid, ax_frame(&element), GeometryKind::Resized))
        }
        "AXTitleChanged" => wid.map(|wid| PlatformEvent::WindowTitleChanged {
            wid,
            title: element
                .string_attribute(crate::ax::ATTR_TITLE)
                .ok()
                .flatten(),
        }),
        "AXFocusedWindowChanged" => {
            let focused = element
                .copy_attribute(ATTR_FOCUSED_WINDOW)
                .ok()
                .and_then(|value| {
                    // SAFETY: borrowed element from a +1 CF value we keep alive.
                    unsafe {
                        AxElement::from_borrowed(
                            objc2_core_foundation::CFRetained::as_ptr(&value)
                                .as_ptr()
                                .cast(),
                        )
                    }
                });
            let focused_wid = focused.as_ref().and_then(|element| element.window_id());
            Some(PlatformEvent::FocusChanged {
                wid: focused_wid,
                pid: Some(context.pid),
            })
        }
        other => ax_state_event(other, wid, context.pid),
    };

    if let Some(event) = event {
        handler(event);
    }
}

struct AxEntry {
    observer: AxObserver,
    app: AxElement,
    context: *mut AxCallbackCtx,
    /// Start time of the observed process, to detect pid reuse.
    started: Option<ProcessStart>,
}

// The observer and elements are immutable after setup.
unsafe impl Send for AxEntry {}

impl AxEntry {
    /// Stop the application's observer and free its callback context.
    fn release(self) {
        let AxEntry {
            observer,
            app,
            context,
            started: _,
        } = self;
        // Invalidate the source first so a notification still queued on the
        // run loop cannot reach `ax_callback` with the freed context.
        observer.invalidate_run_loop_source();
        drop(observer);
        drop(app);
        // SAFETY: the observer is gone, so the context is no longer referenced.
        unsafe { drop(Box::from_raw(context)) };
    }
}

/// Observed applications to release: no longer in the window list **and** the
/// process is gone.  An app that is merely hidden or has no on-screen window
/// keeps its observer.
fn pids_to_prune(
    observed: impl IntoIterator<Item = libc::pid_t>,
    listed: &[libc::pid_t],
    is_dead: impl Fn(libc::pid_t) -> bool,
) -> Vec<libc::pid_t> {
    let mut prune: Vec<libc::pid_t> = observed
        .into_iter()
        .filter(|pid| !listed.contains(pid) && is_dead(*pid))
        .collect();
    prune.sort_unstable();
    prune
}

/// Whether `pid` names no process at all (`kill(pid, 0)` fails with `ESRCH`).
/// `EPERM` means the process exists but belongs to someone else.
fn process_is_dead(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        // 0 and negative values address process groups, not one process.
        return false;
    }
    // SAFETY: signal 0 performs only the existence and permission checks.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Process start time (seconds, microseconds), unique per incarnation of a pid.
type ProcessStart = (u64, u64);

/// Start time of `pid`, or `None` when it cannot be read (process gone).
fn process_start(pid: libc::pid_t) -> Option<ProcessStart> {
    if pid <= 0 {
        return None;
    }
    // SAFETY: proc_bsdinfo is plain old data; zeroed is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a writable buffer of `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (written == size).then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

/// Observed applications whose pid now names a different process than the one
/// the observer was created for (the pid exited and was reused).  An
/// unreadable current start time is not evidence of reuse.
fn pids_reused(
    observed: impl IntoIterator<Item = (libc::pid_t, Option<ProcessStart>)>,
    current: impl Fn(libc::pid_t) -> Option<ProcessStart>,
) -> Vec<libc::pid_t> {
    let mut reused: Vec<libc::pid_t> = observed
        .into_iter()
        .filter(|&(pid, started)| match (started, current(pid)) {
            (Some(then), Some(now)) => then != now,
            _ => false,
        })
        .map(|(pid, _)| pid)
        .collect();
    reused.sort_unstable();
    reused
}

/// Installs the SkyLight and AX event sources and forwards a unified event
/// stream.
///
/// Only one `PlatformObserver` per process is supported (SkyLight has no
/// unregister API and AX callbacks dispatch through a process-wide handler).
/// Create it on the thread whose run loop will be running (the main thread for
/// the daemon).
pub struct PlatformObserver {
    handler: EventHandler,
    apps: HashMap<libc::pid_t, AxEntry>,
    sls_started: bool,
}

impl PlatformObserver {
    /// Create an observer. Call [`PlatformObserver::start_sls`] and
    /// [`PlatformObserver::refresh_apps`] to begin receiving events.
    pub fn new(handler: EventHandler) -> Self {
        let slot = SLS_HANDLER.get_or_init(|| Mutex::new(None));
        *slot.lock().expect("sls handler poisoned") = Some(handler.clone());
        let slot = AX_HANDLER.get_or_init(|| Mutex::new(None));
        *slot.lock().expect("ax handler poisoned") = Some(handler.clone());
        Self {
            handler,
            apps: HashMap::new(),
            sls_started: false,
        }
    }

    /// The handler installed by this observer.
    pub fn handler(&self) -> &EventHandler {
        &self.handler
    }

    /// Register the SkyLight notifications. Safe to call more than once.
    pub fn start_sls(&mut self) -> Result<()> {
        if self.sls_started {
            return Ok(());
        }
        let cid = skylight::main_connection()?;
        for &event in &[
            sls_events::WINDOW_CLOSED,
            sls_events::WINDOW_MOVED,
            sls_events::WINDOW_RESIZED,
            sls_events::WINDOW_REORDERED,
            sls_events::WINDOW_LEVEL_CHANGED,
            sls_events::WINDOW_HIDDEN,
            sls_events::WINDOW_UNHIDDEN,
            sls_events::WINDOW_TITLE_CHANGED,
            sls_events::SPACE_WINDOW_CREATED,
            sls_events::SPACE_WINDOW_DESTROYED,
            sls_events::FRONTMOST_APPLICATION_CHANGED,
        ] {
            // Leak one u32 per registration: SkyLight keeps the context pointer
            // and offers no unregister call.
            let context = Box::into_raw(Box::new(event)).cast::<c_void>();
            // JankyBorders uses the global SLSRegisterNotifyProc variant on
            // modern macOS; the per-connection variant registers successfully
            // but did not deliver window events in the 15.6 VM.
            // SAFETY: callback and leaked context live for the process.
            let result = unsafe { skylight::register_notify_proc(sls_callback, event, context) };
            match result {
                Ok(0) => log::debug!("registered SkyLight event {event}"),
                Ok(code) => log::warn!("SkyLight event {event} registration returned {code}"),
                Err(err) => log::warn!("failed to register SkyLight event {event}: {err}"),
            }
        }
        // Move/resize/title notifications are delivered while the connection's
        // event port is drained; without this pump the callbacks never fire.
        if let Err(err) = skylight::start_event_pump(cid) {
            log::warn!("SkyLight event pump unavailable: {err}");
        }
        self.sls_started = true;
        // Existing windows need an explicit subscription for move/resize events.
        if let Ok(windows) = window::list_windows() {
            let wids: Vec<u32> = windows.iter().map(|window| window.wid).collect();
            let _ = self.request_window_events(&wids);
        }
        Ok(())
    }

    /// Ask SkyLight to deliver move/resize notifications for specific windows.
    ///
    /// The list is capped by SkyLight at 1024 windows per call.
    pub fn request_window_events(&self, wids: &[u32]) -> Result<()> {
        if wids.is_empty() {
            return Ok(());
        }
        let cid = skylight::main_connection()?;
        for chunk in wids.chunks(1024) {
            // SAFETY: valid connection, slice pointer and length.
            let result = unsafe {
                skylight::SLSRequestNotificationsForWindows(cid, chunk.as_ptr(), chunk.len() as i32)
            };
            match result {
                Ok(0) => log::debug!("subscribed to window events for {} windows", chunk.len()),
                Ok(code) => log::warn!("SLSRequestNotificationsForWindows returned {code}"),
                Err(err) => log::warn!("SLSRequestNotificationsForWindows failed: {err}"),
            }
        }
        Ok(())
    }

    /// Start observing `pid` (idempotent).
    pub fn observe_app(&mut self, pid: libc::pid_t) -> Result<()> {
        if self.apps.contains_key(&pid) {
            return Ok(());
        }
        let app = AxElement::application(pid)?;
        let observer = AxObserver::new(pid, ax_callback)?;
        let context = Box::into_raw(Box::new(AxCallbackCtx {
            pid,
            observer: observer.as_ptr(),
        }));

        for notification in APP_NOTIFICATIONS {
            if let Err(err) = observer.add_notification(&app, notification, context.cast()) {
                log::debug!("AX {notification} on pid {pid}: {err}");
            }
        }
        if let Ok(windows) = app.elements_attribute(ATTR_WINDOWS) {
            for window in windows {
                for notification in WINDOW_NOTIFICATIONS {
                    let _ = observer.add_notification(&window, notification, context.cast());
                }
            }
        }
        observer.add_to_current_run_loop()?;
        self.apps.insert(
            pid,
            AxEntry {
                observer,
                app,
                context,
                started: process_start(pid),
            },
        );
        Ok(())
    }

    /// Observe every application that currently owns an on-screen window,
    /// and release the observers of applications that have exited.
    ///
    /// Only the owner pids are enumerated ([`window::list_window_pids`], no AX
    /// enrichment): this runs periodically on the run loop that delivers the
    /// AX callbacks, and per-window AX reads would let one hung application
    /// stall it.  Only a newly seen pid costs AX calls, in `observe_app`.
    ///
    /// Returns the number of listed applications that are observed.
    pub fn refresh_apps(&mut self) -> Result<usize> {
        if !crate::ax::is_process_trusted() {
            return Err(crate::error::PlatformError::AccessibilityDenied);
        }
        let pids = window::list_window_pids()?;
        // A pid that exited and was reused between two refreshes still has its
        // old entry; drop it so the new process is observed below.
        let reused = pids_reused(
            self.apps.iter().map(|(&pid, entry)| (pid, entry.started)),
            process_start,
        );
        for pid in reused {
            if let Some(entry) = self.apps.remove(&pid) {
                log::debug!("pid {pid} was reused; releasing its stale AX observer");
                entry.release();
            }
        }
        let mut observed = 0;
        for &pid in &pids {
            match self.observe_app(pid) {
                Ok(()) => observed += 1,
                Err(err) => log::debug!("cannot observe pid {pid}: {err}"),
            }
        }
        for pid in pids_to_prune(self.apps.keys().copied(), &pids, process_is_dead) {
            if let Some(entry) = self.apps.remove(&pid) {
                log::debug!("pid {pid} exited; releasing its AX observer");
                entry.release();
            }
        }
        Ok(observed)
    }

    /// Number of observed applications.
    pub fn observed_app_count(&self) -> usize {
        self.apps.len()
    }
}

impl Drop for PlatformObserver {
    fn drop(&mut self) {
        if let Some(slot) = SLS_HANDLER.get() {
            *slot.lock().expect("sls handler poisoned") = None;
        }
        if let Some(slot) = AX_HANDLER.get() {
            *slot.lock().expect("ax handler poisoned") = None;
        }
        // Release each AXObserver before reclaiming its callback context so no
        // callback can observe a dangling pointer.
        for (_, entry) in self.apps.drain() {
            entry.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ax_state_notifications_map_to_events() {
        let pid = 42;
        assert_eq!(
            ax_state_event("AXApplicationHidden", None, pid),
            Some(PlatformEvent::AppHidden { pid })
        );
        assert_eq!(
            ax_state_event("AXApplicationShown", None, pid),
            Some(PlatformEvent::AppShown { pid })
        );
        assert_eq!(
            ax_state_event("AXApplicationActivated", None, pid),
            Some(PlatformEvent::AppActivated { pid: Some(pid) })
        );
        assert_eq!(
            ax_state_event("AXWindowMiniaturized", Some(7), pid),
            Some(PlatformEvent::WindowMinimized { wid: 7 })
        );
        assert_eq!(
            ax_state_event("AXWindowDeminiaturized", Some(7), pid),
            Some(PlatformEvent::WindowDeminimized { wid: 7 })
        );
        assert_eq!(
            ax_state_event("AXUIElementDestroyed", Some(7), pid),
            Some(PlatformEvent::WindowDestroyed { wid: 7 })
        );
        // Window notifications need the element's window id.
        assert_eq!(ax_state_event("AXWindowMiniaturized", None, pid), None);
        assert_eq!(ax_state_event("AXWindowDeminiaturized", None, pid), None);
        assert_eq!(ax_state_event("AXUIElementDestroyed", None, pid), None);
        // Unknown notifications are dropped.
        assert_eq!(ax_state_event("AXSomethingElse", Some(7), pid), None);
        // Every registered notification is decoded somewhere: the state ones
        // here, the rest (which need element reads) in `ax_callback`.
        let decoded_in_callback = [
            "AXWindowCreated",
            "AXFocusedWindowChanged",
            "AXWindowMoved",
            "AXWindowResized",
            "AXTitleChanged",
        ];
        for name in APP_NOTIFICATIONS.iter().chain(WINDOW_NOTIFICATIONS) {
            assert!(
                decoded_in_callback.contains(name) || ax_state_event(name, Some(7), pid).is_some(),
                "{name} is registered but never decoded"
            );
        }
    }

    #[test]
    fn unreadable_bounds_drop_the_geometry_event() {
        let frame = Rect::new(1.0, 2.0, 30.0, 40.0);
        assert_eq!(geometry_event(7, None, GeometryKind::Moved), None);
        assert_eq!(geometry_event(7, None, GeometryKind::Resized), None);
        assert_eq!(
            geometry_event(7, Some(frame), GeometryKind::Moved),
            Some(PlatformEvent::WindowMoved { wid: 7, frame })
        );
        assert_eq!(
            geometry_event(7, Some(frame), GeometryKind::Resized),
            Some(PlatformEvent::WindowResized { wid: 7, frame })
        );
    }

    #[test]
    fn only_a_changed_start_time_marks_a_reused_pid() {
        let now = |pid: libc::pid_t| match pid {
            10 => Some((100, 5)), // same incarnation
            20 => Some((200, 9)), // reused
            _ => None,            // unreadable: not evidence
        };
        let reused = pids_reused(
            [
                (20, Some((100, 0))),
                (10, Some((100, 5))),
                (30, Some((1, 1))),
                (40, None),
            ],
            now,
        );
        assert_eq!(reused, vec![20]);
    }

    #[test]
    fn this_process_has_a_stable_start_time() {
        let pid = std::process::id() as libc::pid_t;
        let first = process_start(pid).expect("own start time");
        assert_eq!(process_start(pid), Some(first));
        assert_eq!(process_start(0), None);
    }

    #[test]
    fn only_unlisted_dead_applications_are_pruned() {
        let dead = [30, 40];
        let prune = pids_to_prune([40, 10, 20, 30], &[10, 30], |pid| dead.contains(&pid));
        // 30 is dead but still listed (a stale window record); 20 is alive
        // with no on-screen window (hidden app).  Only 40 goes.
        assert_eq!(prune, vec![40]);
        assert!(pids_to_prune([], &[10], |_| true).is_empty());
        assert!(pids_to_prune([10, 20], &[], |_| false).is_empty());
    }

    #[test]
    fn a_reaped_child_is_dead_and_this_process_is_not() {
        assert!(!process_is_dead(std::process::id() as libc::pid_t));
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn /usr/bin/true");
        let pid = child.id() as libc::pid_t;
        child.wait().expect("wait for the child");
        assert!(process_is_dead(pid), "a reaped child must be ESRCH");
        assert!(!process_is_dead(0), "process groups are never pruned");
        assert!(!process_is_dead(-1));
    }
}
