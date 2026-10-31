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
    /// A window was hidden or minimized.
    WindowHidden {
        /// Window ID.
        wid: u32,
    },
    /// A window was shown or deminimized.
    WindowShown {
        /// Window ID.
        wid: u32,
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
        sls_events::WINDOW_MOVED => read_u32(bytes, 0).map(|wid| {
            let frame = skylight::window_bounds(wid)
                .map(Rect::from_cg)
                .unwrap_or_default();
            PlatformEvent::WindowMoved { wid, frame }
        }),
        sls_events::WINDOW_RESIZED => read_u32(bytes, 0).map(|wid| {
            let frame = skylight::window_bounds(wid)
                .map(Rect::from_cg)
                .unwrap_or_default();
            PlatformEvent::WindowResized { wid, frame }
        }),
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
        "AXWindowMoved" => wid.map(|wid| PlatformEvent::WindowMoved {
            wid,
            frame: element.frame().ok().flatten().unwrap_or_default(),
        }),
        "AXWindowResized" => wid.map(|wid| PlatformEvent::WindowResized {
            wid,
            frame: element.frame().ok().flatten().unwrap_or_default(),
        }),
        "AXTitleChanged" => wid.map(|wid| PlatformEvent::WindowTitleChanged {
            wid,
            title: element
                .string_attribute(crate::ax::ATTR_TITLE)
                .ok()
                .flatten(),
        }),
        "AXWindowMiniaturized" => wid.map(|wid| PlatformEvent::WindowHidden { wid }),
        "AXWindowDeminiaturized" => wid.map(|wid| PlatformEvent::WindowShown { wid }),
        "AXUIElementDestroyed" => wid.map(|wid| PlatformEvent::WindowDestroyed { wid }),
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
        "AXApplicationActivated" => Some(PlatformEvent::AppActivated {
            pid: Some(context.pid),
        }),
        "AXApplicationHidden" | "AXApplicationShown" => None,
        _ => None,
    };

    if let Some(event) = event {
        handler(event);
    }
}

struct AxEntry {
    observer: AxObserver,
    app: AxElement,
    context: *mut AxCallbackCtx,
}

// The observer and elements are immutable after setup.
unsafe impl Send for AxEntry {}

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
            },
        );
        Ok(())
    }

    /// Observe every application that currently owns an on-screen window.
    pub fn refresh_apps(&mut self) -> Result<usize> {
        if !crate::ax::is_process_trusted() {
            return Err(crate::error::PlatformError::AccessibilityDenied);
        }
        let windows = window::list_windows()?;
        let mut pids: Vec<libc::pid_t> = windows.iter().map(|window| window.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        let mut observed = 0;
        for pid in pids {
            match self.observe_app(pid) {
                Ok(()) => observed += 1,
                Err(err) => log::debug!("cannot observe pid {pid}: {err}"),
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
        // Dropping AxEntry releases the AXObserver; reclaim the callback context
        // afterwards so no callback can observe a dangling pointer.
        for (_, entry) in self.apps.drain() {
            drop(entry.observer);
            drop(entry.app);
            // SAFETY: observer is gone, so the context is no longer referenced.
            unsafe { drop(Box::from_raw(entry.context)) };
        }
    }
}
