// SPDX-License-Identifier: GPL-3.0-or-later
//! Tokio event loop: joins platform events, key actions and IPC requests into
//! the single-threaded [`Core`].

use std::path::{Path, PathBuf};

use cosmic_mac_platform::PlatformEvent;
use cosmic_mac_protocol::{Action, Config, Rect, Request, Response};
use tokio::sync::{mpsc, oneshot};

use crate::backend::SharedBackend;
use crate::core::Core;
use crate::ipc;
/// A message for the layout owner.
pub enum Message {
    /// A window/application/display event from the platform observers.
    Platform(PlatformEvent),
    /// A bound key combination.
    Key(Action),
    /// The main display's work area changed.
    WorkArea(Rect),
    /// An IPC request and the channel for its reply.
    Ipc(Request, oneshot::Sender<Response>),
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
    tokio::spawn(async move {
        ipc::serve(listener, ipc_tx).await;
    });

    let signal_tx = tx.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        log::info!("shutdown signal received");
        let _ = signal_tx.send(Message::Shutdown);
    });

    let windows = core.applier().backend().windows();
    let count = windows.len();
    let report = core.startup(windows);
    log::info!(
        "startup enumeration: {} managed windows ({count} on screen), tree order {:?}",
        report.added,
        report.order
    );

    while let Some(message) = rx.recv().await {
        match message {
            Message::Platform(event) => core.platform_event(event),
            Message::Key(action) => core.action(action),
            Message::WorkArea(rect) => core.set_work_area(rect),
            Message::Ipc(request, response) => {
                let reply = core.request(request);
                let _ = response.send(reply);
            }
            Message::Shutdown => break,
        }
    }

    core.shutdown();
    remove_socket(&socket_path);
    Ok(())
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
