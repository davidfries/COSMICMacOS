// SPDX-License-Identifier: GPL-3.0-or-later
//! Entry point for the cosmicMac window-manager daemon.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cosmic_mac_platform::eventtap::{self, TapHandler};
use cosmic_mac_platform::{EventTap, KeyEvent, MouseTap, PlatformObserver, ax, display, tcc};
use cosmic_mac_protocol::{Config, Rect};
use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::{CFRunLoop, CFRunLoopRunResult, kCFRunLoopDefaultMode};
use tokio::sync::mpsc;

use cosmic_mac_wm::daemon::Message;
use cosmic_mac_wm::keybindings::{self, KeyMap};
use cosmic_mac_wm::launcher::{LauncherClient, SocketPort};
use cosmic_mac_wm::macos::{self, DisplayUpdate, MacPlatform};
use cosmic_mac_wm::super_tap::{self, SuperTap, TapEvent};
use cosmic_mac_wm::{daemon, ipc, journal, paths};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if let Err(error) = tcc::require_accessibility() {
        eprintln!(
            "\n!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n\
             !! cosmic-mac-wm cannot start: {error}\n\
             !! Grant Accessibility in System Settings > Privacy & Security >\n\
             !! Accessibility for this signed binary, then run it again.\n\
             !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n"
        );
        std::process::exit(1);
    }

    // A slow or beach-balling app now fails our AX calls after
    // AX_MESSAGING_TIMEOUT_SECS (1 s) instead of stalling the daemon for the
    // ~6 s system default on every call.
    if let Err(error) = ax::set_global_messaging_timeout(ax::AX_MESSAGING_TIMEOUT_SECS) {
        log::warn!("cannot set the AX messaging timeout: {error}; AX calls keep the ~6 s default");
    }

    if display::screens_have_separate_spaces() == Some(true) {
        log::warn!(
            "'Displays have separate Spaces' is ON; corner parking can leak windows across \
             displays. Turn it off in System Settings > Desktop & Dock."
        );
    }

    let config = load_config();
    let (keymap, warnings) = KeyMap::load(Some(&paths::keybindings_path()));
    for warning in warnings {
        log::warn!("keybindings: {warning}");
    }
    log::info!("{} keybindings armed", keymap.len());

    // Refuse a second daemon before the display wait below, which can last
    // as long as the display sleeps; checked again after it.
    exit_if_already_running();

    // The backend and the observers must be created on the main thread: NSScreen
    // work areas and the AX observer run loop live here.  The event tap runs on
    // its own thread (see `spawn_event_tap`).  Nothing is managed until a real
    // display is active.
    let (work_area, main_frame) = wait_for_main_display();
    exit_if_already_running();
    let backend = Arc::new(MacPlatform::new(work_area, main_frame));

    let restored = journal::restore_stale(&paths::journal_path(), backend.as_ref());
    if restored > 0 {
        log::info!("restored {restored} windows from a crashed run");
    }

    let launcher = start_launcher_client(config.super_tap_launcher);
    let (tx, rx) = mpsc::unbounded_channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let (observer, tap_thread) = match start_observers(
        tx.clone(),
        Arc::new(keymap),
        Arc::new(config.passthrough.clone()),
        launcher,
        backend.clone(),
        shutdown.clone(),
    ) {
        Ok(started) => started,
        Err(error) => {
            log::error!("cannot start platform observers: {error}");
            std::process::exit(1);
        }
    };

    let runtime_shutdown = shutdown.clone();
    let socket_path = paths::socket_path();
    let journal_path = paths::journal_path();
    let session_path = paths::session_path();
    let backend_for_run: cosmic_mac_wm::backend::SharedBackend = backend.clone();
    let runtime_thread = std::thread::Builder::new()
        .name("wm-runtime".into())
        .spawn(move || {
            // Held for the whole thread body: a normal return *and* a panic
            // unwinding out of the runtime both stop the main thread's run loop.
            // Without it a panic left a zombie whose event tap kept swallowing
            // every bound chord system-wide.
            let _shutdown = ShutdownOnDrop(runtime_shutdown);
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    log::error!("cannot create tokio runtime: {error}");
                    return;
                }
            };
            let launch = daemon::Launch {
                backend: backend_for_run,
                config,
                work_area,
                journal_path,
                session_path,
                socket_path,
            };
            if let Err(error) = runtime.block_on(daemon::run(launch, rx, tx)) {
                log::error!("daemon stopped: {error}");
            }
        })
        .expect("cannot spawn the daemon thread");

    log::info!("cosmic-mac-wm running");
    observer.run(shutdown);
    let _ = runtime_thread.join();
    // The tap thread polls the same shutdown flag and releases the tap.
    let _ = tap_thread.join();
    log::info!("cosmic-mac-wm exited");
}

/// The Super-tap launcher client, unless `super_tap_launcher` is off in the
/// WM config. The launcher is started on demand from the directory of this
/// executable (`cosmic-mac-launcher.app`, else `cosmic-mac-launcher`).
fn start_launcher_client(enabled: bool) -> Option<LauncherClient> {
    if !enabled {
        log::info!("Super tap: off (super_tap_launcher: false)");
        return None;
    }
    let bin_dir = executable_dir().unwrap_or_else(|error| {
        log::warn!(
            "cannot locate the running executable ({error}); the launcher cannot be \
             started on demand"
        );
        PathBuf::new()
    });
    let port = SocketPort {
        socket: paths::launcher_socket_path(),
        bin_dir,
        log: Some(paths::launcher_log_path()),
    };
    log::info!(
        "Super tap toggles the launcher ({}; on demand from {})",
        port.socket.display(),
        port.bin_dir.display()
    );
    match LauncherClient::spawn(port) {
        Ok(client) => Some(client),
        Err(error) => {
            log::warn!("Super tap: off (cannot start the launcher-client thread: {error})");
            None
        }
    }
}

fn executable_dir() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.parent().map(Path::to_path_buf).unwrap_or_default())
}

fn exit_if_already_running() {
    if ipc::is_listening(&paths::socket_path()) {
        eprintln!(
            "cosmic-mac-wm: another daemon is already listening on {}; refusing to start",
            paths::socket_path().display()
        );
        std::process::exit(1);
    }
}

/// Run loop slice between two polls of the startup display wait.
const DISPLAY_POLL_SECS: f64 = 0.5;
/// How often the startup display wait logs that it is still waiting.
const DISPLAY_WAIT_REPORT: Duration = Duration::from_secs(60);

/// Block until the main display is active and return its `(work_area,
/// frame)`.
///
/// A sleeping display enumerates as zero displays.  Starting against an
/// invented area tiles past the real screen edge and learns that edge as
/// application size limits, so there is no timeout.  The main run loop runs
/// in short slices between polls because AppKit refreshes `NSScreen` (the work
/// area) from notifications delivered there; a plain sleep loop could see
/// CoreGraphics wake up while `NSScreen` stays empty.
///
/// No signal handler is installed yet (the daemon's tokio handler comes
/// later), so SIGTERM/SIGINT keep their default disposition and end the
/// process at once during the wait.  Nothing needs restoring then: no window
/// has been touched, no socket bound, no journal written.
fn wait_for_main_display() -> (Rect, Rect) {
    let started = Instant::now();
    let mut active_since: Option<Instant> = None;
    let mut last_report: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let mut error = None;
        let enumerated = match display::active_display_count() {
            Ok(0) => None,
            Ok(_) => macos::main_display_frames_now(),
            Err(count_error) => {
                error = Some(count_error.to_string());
                None
            }
        };
        if enumerated.is_some() {
            active_since.get_or_insert(now);
        } else {
            active_since = None;
        }
        let active_for = active_since.map_or(Duration::ZERO, |since| now - since);
        if let Some(frames) = macos::startup_display_frames(enumerated, active_for) {
            if !frames.work_area_known {
                log::warn!(
                    "NSScreen does not list the main display {:?}; using its full frame as \
                     the work area until the next display change",
                    frames.frame
                );
            }
            if last_report.is_some() {
                log::info!(
                    "display active after {}s; work area {:?}",
                    started.elapsed().as_secs(),
                    frames.work_area
                );
            }
            return frames.pair();
        }
        let waiting_for = match (&enumerated, &error) {
            (Some(_), _) => "NSScreen to list the main display".to_string(),
            (None, Some(error)) => format!("an active display ({error})"),
            (None, None) => "an active display (asleep?)".to_string(),
        };
        match last_report {
            None => {
                log::warn!("waiting for {waiting_for} before managing windows");
                last_report = Some(now);
            }
            Some(reported) if now - reported >= DISPLAY_WAIT_REPORT => {
                log::info!(
                    "still waiting for {waiting_for} ({}s)",
                    started.elapsed().as_secs()
                );
                last_report = Some(now);
            }
            Some(_) => {}
        }
        let mode = unsafe { kCFRunLoopDefaultMode };
        if CFRunLoop::run_in_mode(mode, DISPLAY_POLL_SECS, false) == CFRunLoopRunResult::Finished {
            // No source in the mode yet: returns at once, so never spin.
            std::thread::sleep(Duration::from_secs_f64(DISPLAY_POLL_SECS));
        }
    }
}

/// Sets the shutdown flag when dropped, so the main thread stops pumping its
/// run loop and the tap thread releases the event tap, however the runtime
/// thread ends.
struct ShutdownOnDrop(Arc<AtomicBool>);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The AX/SkyLight observers, owned by the main thread's run loop.
struct ObserverRuntime {
    observer: PlatformObserver,
}

impl ObserverRuntime {
    fn run(mut self, shutdown: Arc<AtomicBool>) {
        let mut ticks = 0u32;
        // The AX observer sources are attached to this run loop; pumping it in
        // short slices also lets us refresh the observed app set periodically.
        // The refresh only enumerates window owner pids (no AX reads), so it
        // cannot block on a hung application.
        while !shutdown.load(Ordering::SeqCst) {
            let mode = unsafe { kCFRunLoopDefaultMode };
            CFRunLoop::run_in_mode(mode, 0.25, false);
            ticks = ticks.wrapping_add(1);
            if ticks.is_multiple_of(8)
                && let Err(error) = self.observer.refresh_apps()
            {
                log::debug!("refreshing AX observers failed: {error}");
            }
        }
    }
}

fn start_observers(
    tx: mpsc::UnboundedSender<Message>,
    keymap: Arc<KeyMap>,
    passthrough: Arc<Vec<String>>,
    launcher: Option<LauncherClient>,
    backend: Arc<MacPlatform>,
    shutdown: Arc<AtomicBool>,
) -> Result<(ObserverRuntime, JoinHandle<()>), String> {
    let event_tx = tx.clone();
    let handler: cosmic_mac_platform::observers::EventHandler = Arc::new(move |event| {
        // Stamp the event when the platform callback fires, before it waits in
        // the channel behind a long AX batch.
        let captured = Instant::now();
        let _ = event_tx.send(Message::Platform(event, captured));
    });
    let mut observer = PlatformObserver::new(handler);
    observer
        .start_sls()
        .map_err(|error| format!("SkyLight notifications: {error}"))?;
    observer
        .refresh_apps()
        .map_err(|error| format!("AX observers: {error}"))?;
    // Native full screen and desktop switches: the core re-checks windows
    // away in full screen and re-asserts the tiles on every change.
    observer.start_space_observer();

    let tap_thread = spawn_event_tap(tx.clone(), keymap, passthrough, launcher, shutdown)?;

    let display_tx = tx.clone();
    display::set_display_change_handler(Arc::new(move || {
        let previous = backend.display_frames();
        match macos::next_display_frames(previous, macos::main_display_frames_now()) {
            DisplayUpdate::Apply { work_area, frame } => {
                backend.update_displays(work_area, frame);
                let _ = display_tx.send(Message::WorkArea(work_area));
            }
            DisplayUpdate::NoDisplay => log::info!(
                "display change with no active display (asleep?); keeping the work area {:?}",
                previous.0
            ),
            DisplayUpdate::WorkAreaPending => log::info!(
                "display change before NSScreen lists the display; keeping the work area {:?}",
                previous.0
            ),
            DisplayUpdate::Unchanged => log::debug!("display change; geometry unchanged"),
        }
    }))
    .map_err(|error| format!("display change handler: {error}"))?;

    Ok((ObserverRuntime { observer }, tap_thread))
}

/// Run the global event tap on its own thread and run loop.
///
/// The main run loop also delivers the AX observer callbacks, and AX calls
/// block until the target application answers.  While the tap shared that
/// loop, one beach-balling app stalled keyboard input system-wide until macOS
/// disabled the tap.  Creation errors are sent back so they still fail daemon
/// startup.
///
/// The listen-only mouse tap for drag to stack shares the thread.  Without it
/// the daemon still runs; tiled windows then always snap back.
fn spawn_event_tap(
    tx: mpsc::UnboundedSender<Message>,
    keymap: Arc<KeyMap>,
    passthrough: Arc<Vec<String>>,
    launcher: Option<LauncherClient>,
    shutdown: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new()
        .name("event-tap".into())
        .spawn(move || {
            // `EventTap::new` adds its source to the *current* run loop, which
            // is this thread's.
            let mouse_tx = tx.clone();
            let handler = key_handler(tx, keymap, passthrough, launcher);
            let tap = match EventTap::new(Vec::new(), handler) {
                Ok(tap) => tap,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("global event tap: {error}")));
                    return;
                }
            };
            let mouse = MouseTap::new(Box::new(move |event| {
                let _ = mouse_tx.send(Message::Mouse(event, Instant::now()));
            }))
            .map_err(|error| log::warn!("mouse tap: {error}; drag to stack is off"))
            .ok();
            let _ = ready_tx.send(Ok(()));
            while !shutdown.load(Ordering::SeqCst) {
                let mode = unsafe { kCFRunLoopDefaultMode };
                if CFRunLoop::run_in_mode(mode, 0.5, false) == CFRunLoopRunResult::Finished {
                    // No source left in the mode (not expected while the tap
                    // lives); never spin.
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            drop(mouse);
            drop(tap);
        })
        .map_err(|error| format!("cannot spawn the event-tap thread: {error}"))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err("the event-tap thread exited during setup".into())
        }
    }
}

/// The tap callback: dispatch bound chords to the daemon and swallow them,
/// and watch every key and modifier event for a Super tap.
fn key_handler(
    tap_tx: mpsc::UnboundedSender<Message>,
    tap_keymap: Arc<KeyMap>,
    tap_passthrough: Arc<Vec<String>>,
    launcher: Option<LauncherClient>,
) -> TapHandler {
    // Only the tap thread calls the handler.
    let detector = RefCell::new(SuperTap::default());
    // The static swallow list is empty: the handler decides per event, which
    // is what makes the per-application `passthrough` list possible.
    Box::new(move |key| {
        if let Some(launcher) = &launcher {
            watch_super_tap(&detector, launcher, &tap_passthrough, key);
        }
        if key.is_flags_changed {
            // Modifier changes always pass (the tap ignores the result too).
            return false;
        }
        let Some(action) = tap_keymap.lookup(key.keycode, key.modifiers()).cloned() else {
            return false;
        };
        if is_passthrough(&tap_passthrough) {
            return false;
        }
        // Autorepeat only re-sends directional focus/move/resize; holding a
        // one-shot chord (Super+Q) must not close window after window.
        // Repeats that are not dispatched are still swallowed below, so
        // the app never sees them either.
        if keybindings::dispatches(&action, key.is_key_down, key.is_repeat) {
            if tap_tx.send(Message::Key(action)).is_err() {
                // The daemon is gone: never swallow keys on its behalf.
                return false;
            }
        } else if tap_tx.is_closed() {
            return false;
        }
        // Swallow both down and up so the app never sees a stuck key.
        true
    })
}

/// Feeds `key` to the Super-tap detector; on a confirmed tap outside the
/// `passthrough` apps, queues a launcher toggle (never blocks the tap).
fn watch_super_tap(
    detector: &RefCell<SuperTap>,
    launcher: &LauncherClient,
    passthrough: &[String],
    key: KeyEvent,
) {
    let event = if key.is_flags_changed {
        TapEvent::Flags {
            keycode: key.keycode,
            flags: key.flags,
        }
    } else {
        TapEvent::Key
    };
    let Some(held) = detector.borrow_mut().observe(event, Instant::now()) else {
        return;
    };
    let since_input =
        super_tap::duration_from_seconds(eventtap::seconds_since_click_scroll_or_key());
    let button_down = eventtap::mouse_button_down();
    if !super_tap::should_toggle(held, since_input, button_down) {
        log::debug!(
            "Super tap ignored: click, scroll or key {since_input:?} ago, held {held:?}, \
             mouse button down: {button_down}"
        );
        return;
    }
    if is_passthrough(passthrough) {
        log::debug!("Super tap ignored: the frontmost app is in `passthrough`");
        return;
    }
    log::debug!("Super tap (held {held:?})");
    if !launcher.toggle() {
        log::warn!("Super tap: the launcher-client thread is gone");
    }
}

fn is_passthrough(patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let Some(app_id) = frontmost_bundle_id() else {
        return false;
    };
    patterns.iter().any(|pattern| app_matches(pattern, &app_id))
}

/// Called from the tap thread.  `NSWorkspace` is not main-thread-only in
/// objc2-app-kit (no `MainThreadMarker`); its `frontmostApplication` is
/// refreshed by the main run loop, which the daemon keeps pumping.
fn frontmost_bundle_id() -> Option<String> {
    let workspace = NSWorkspace::sharedWorkspace();
    let app = workspace.frontmostApplication()?;
    app.bundleIdentifier().map(|bundle| bundle.to_string())
}

/// Case-insensitive substring / `*` wildcard match, matching the config's
/// `tiling_exceptions` semantics.
fn app_matches(pattern: &str, app_id: &str) -> bool {
    let pattern = pattern.trim().to_lowercase();
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    let app_id = app_id.to_lowercase();
    if !pattern.contains('*') {
        return app_id.contains(&pattern);
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut position = 0usize;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            if !app_id.starts_with(part) {
                return false;
            }
            position = part.len();
        } else if index == parts.len() - 1 {
            return app_id[position..].ends_with(part);
        } else if let Some(found) = app_id[position..].find(part) {
            position += found + part.len();
        } else {
            return false;
        }
    }
    true
}

fn load_config() -> Config {
    let path = paths::wm_config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match ron::from_str::<Config>(&text) {
            Ok(config) => {
                log::info!("loaded WM config from {}", path.display());
                config
            }
            Err(error) => {
                log::warn!("cannot parse {}: {error}; using defaults", path.display());
                Config::default()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(error) => {
            log::warn!("cannot read {}: {error}; using defaults", path.display());
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_guard_sets_the_flag_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        drop(ShutdownOnDrop(flag.clone()));
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn shutdown_guard_sets_the_flag_when_the_thread_panics() {
        let flag = Arc::new(AtomicBool::new(false));
        let guard_flag = flag.clone();
        let result = std::thread::spawn(move || {
            let _shutdown = ShutdownOnDrop(guard_flag);
            panic!("runtime crashed");
        })
        .join();
        assert!(result.is_err(), "the thread must have panicked");
        assert!(flag.load(Ordering::SeqCst));
    }
}
