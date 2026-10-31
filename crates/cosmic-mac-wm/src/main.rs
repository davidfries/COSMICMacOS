// SPDX-License-Identifier: GPL-3.0-or-later
//! Entry point for the cosmicMac window-manager daemon.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cosmic_mac_platform::{EventTap, PlatformObserver, display, tcc};
use cosmic_mac_protocol::Config;
use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use tokio::sync::mpsc;

use cosmic_mac_wm::backend::PlatformBackend;
use cosmic_mac_wm::daemon::Message;
use cosmic_mac_wm::keybindings::{self, KeyMap};
use cosmic_mac_wm::macos::{self, MacPlatform};
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

    // The backend and the observers must be created on the main thread: NSScreen
    // work areas and the AX/event-tap run loop live here.
    let backend = Arc::new(MacPlatform::new());
    let work_area = backend.main_work_area();

    if ipc::is_listening(&paths::socket_path()) {
        eprintln!(
            "cosmic-mac-wm: another daemon is already listening on {}; refusing to start",
            paths::socket_path().display()
        );
        std::process::exit(1);
    }

    let restored = journal::restore_stale(&paths::journal_path(), backend.as_ref());
    if restored > 0 {
        log::info!("restored {restored} windows from a crashed run");
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let observer = match start_observers(
        tx.clone(),
        Arc::new(keymap),
        Arc::new(config.passthrough.clone()),
        backend.clone(),
    ) {
        Ok(observer) => observer,
        Err(error) => {
            log::error!("cannot start platform observers: {error}");
            std::process::exit(1);
        }
    };

    let shutdown = Arc::new(AtomicBool::new(false));
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
    log::info!("cosmic-mac-wm exited");
}

/// Sets the shutdown flag when dropped, so the main thread stops pumping its
/// run loop (and releases the event tap) however the runtime thread ends.
struct ShutdownOnDrop(Arc<AtomicBool>);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Observers plus the event tap, owned by the main thread's run loop.
struct ObserverRuntime {
    observer: PlatformObserver,
    _tap: EventTap,
}

impl ObserverRuntime {
    fn run(mut self, shutdown: Arc<AtomicBool>) {
        let mut ticks = 0u32;
        // The AX observer sources are attached to this run loop; pumping it in
        // short slices also lets us refresh the observed app set periodically.
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
    backend: Arc<MacPlatform>,
) -> Result<ObserverRuntime, String> {
    let event_tx = tx.clone();
    let handler: cosmic_mac_platform::observers::EventHandler = Arc::new(move |event| {
        let _ = event_tx.send(Message::Platform(event));
    });
    let mut observer = PlatformObserver::new(handler);
    observer
        .start_sls()
        .map_err(|error| format!("SkyLight notifications: {error}"))?;
    observer
        .refresh_apps()
        .map_err(|error| format!("AX observers: {error}"))?;

    let tap_tx = tx.clone();
    let tap_keymap = keymap.clone();
    let tap_passthrough = passthrough.clone();
    // The static swallow list is empty: the handler decides per event, which
    // is what makes the per-application `passthrough` list possible.
    let tap = EventTap::new(
        Vec::new(),
        Box::new(move |key| {
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
        }),
    )
    .map_err(|error| format!("global event tap: {error}"))?;

    let display_tx = tx.clone();
    display::set_display_change_handler(Arc::new(move || {
        let (work_area, main_frame) = macos::main_display_frames_now();
        backend.update_displays(work_area, main_frame);
        let _ = display_tx.send(Message::WorkArea(work_area));
    }))
    .map_err(|error| format!("display change handler: {error}"))?;

    Ok(ObserverRuntime {
        observer,
        _tap: tap,
    })
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
