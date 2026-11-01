// SPDX-License-Identifier: GPL-3.0-or-later
//! Tokio event loop: joins platform events, key actions and IPC requests into
//! the single-threaded [`Core`].

use std::path::{Path, PathBuf};
use std::time::Instant;

use cosmic_mac_platform::{MouseEvent, PlatformEvent};
use cosmic_mac_protocol::{Action, Config, Rect, Request, Response, WindowId};
use tokio::sync::{mpsc, oneshot};

use crate::backend::SharedBackend;
use crate::core::Core;
use crate::ipc;
use crate::shell_state;
/// A message for the layout owner.
pub enum Message {
    /// A window/application/display event from the platform observers, with
    /// the time the platform callback fired.  Echo suppression and the focus
    /// debounce compare against that capture time, not the time the event is
    /// processed, because events queue while the daemon runs long AX batches.
    Platform(PlatformEvent, Instant),
    /// A bound key combination.
    Key(Action),
    /// A left-button event from the listen-only mouse tap, with the time the
    /// tap saw it (drag to stack).
    Mouse(MouseEvent, Instant),
    /// The main display's work area changed.
    WorkArea(Rect),
    /// An IPC request and the channel for its reply.
    Ipc(Request, oneshot::Sender<Response>),
    /// A re-check the core asked for ([`Core::take_rechecks`]) is due: look
    /// at a window whose state could not be read (a native full-screen
    /// transition) again.
    Recheck(WindowId),
    /// SIGTERM/SIGINT or an unrecoverable startup failure.
    Shutdown,
}

/// Everything the daemon needs to start.
pub struct Launch {
    /// Platform backend.
    pub backend: SharedBackend,
    /// Window-manager configuration.
    pub config: Config,
    /// Work area at startup.
    pub work_area: Rect,
    /// Crash journal path.
    pub journal_path: PathBuf,
    /// Layout session snapshot path.
    pub session_path: PathBuf,
    /// IPC socket path.
    pub socket_path: PathBuf,
}

/// Run the daemon until a signal arrives or the message channel closes.
pub async fn run(
    launch: Launch,
    mut rx: mpsc::UnboundedReceiver<Message>,
    tx: mpsc::UnboundedSender<Message>,
) -> Result<(), String> {
    let Launch {
        backend,
        config,
        work_area,
        journal_path,
        session_path,
        socket_path,
    } = launch;
    let mut core = Core::new(config, work_area, backend, journal_path, session_path);

    let listener = ipc::bind(&socket_path)?;
    let ipc_tx = tx.clone();
    // Shell-state stream for `Request::Subscribe` (the menu-bar status item):
    // updated after startup and after every message; subscribers hear changes.
    let shell_state = shell_state::Publisher::new(core.layout());
    let ipc_shell_state = shell_state.subscribe();
    tokio::spawn(async move {
        ipc::serve(listener, ipc_tx, ipc_shell_state).await;
    });

    let signal_tx = tx.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        log::info!("shutdown signal received");
        let _ = signal_tx.send(Message::Shutdown);
    });

    // A panel that is already running sits above the normal level, so it is
    // not in `windows()`.  Reserve its strip first so the initial layout tiles
    // below it and the session records the effective work area.
    let shell_windows = core.applier().backend().shell_windows();
    core.track_shell_windows(shell_windows);
    let windows = core.applier().backend().windows();
    let count = windows.len();
    let report = core.startup(windows);
    log::info!(
        "startup enumeration: {} managed windows ({count} on screen), tree order {:?}",
        report.added,
        report.order
    );
    schedule_rechecks(&mut core, &tx);
    shell_state.update(core.layout());

    while let Some(message) = rx.recv().await {
        match message {
            Message::Platform(event, captured) => {
                log::trace!("platform event: {event:?}");
                core.platform_event_at(event, captured);
            }
            Message::Key(action) => core.action(action),
            Message::Mouse(event, captured) => core.mouse_event_at(event, captured),
            Message::WorkArea(rect) => core.set_work_area(rect),
            Message::Ipc(request, response) => {
                let reply = core.request(request);
                let _ = response.send(reply);
            }
            Message::Recheck(wid) => core.recheck(wid),
            Message::Shutdown => break,
        }
        schedule_rechecks(&mut core, &tx);
        shell_state.update(core.layout());
    }

    core.shutdown();
    remove_socket(&socket_path);
    Ok(())
}

/// Start a timer for every re-check the core requested; each sends
/// [`Message::Recheck`] when it is due.  The core never sleeps, so it stays
/// single-threaded and testable.
fn schedule_rechecks(core: &mut Core, tx: &mpsc::UnboundedSender<Message>) {
    for (wid, delay) in core.take_rechecks() {
        let tx = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(Message::Recheck(wid));
        });
    }
}

fn remove_socket(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("cannot remove socket {}: {error}", path.display());
    }
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                log::warn!("cannot install SIGTERM handler: {error}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
