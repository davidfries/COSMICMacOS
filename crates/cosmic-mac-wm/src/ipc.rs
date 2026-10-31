// SPDX-License-Identifier: GPL-3.0-or-later
//! Tokio Unix-socket IPC server.
//!
//! One JSON [`Request`] per line, one JSON [`Response`] per line.  Requests are
//! forwarded to the single-threaded layout owner and the reply is awaited on a
//! oneshot channel, so queries always see a consistent snapshot.

use std::path::Path;

use cosmic_mac_protocol::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::daemon::Message;

/// Whether a live daemon is already listening on `path`.
///
/// Used before replaying the crash journal: a second daemon must never restore
/// the frames of a run that is still managing them.
pub fn is_listening(path: &Path) -> bool {
    path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Bind the daemon socket, replacing a stale socket left by a crash.
pub fn bind(path: &Path) -> Result<UnixListener, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => {
                return Err(format!(
                    "another cosmic-mac-wm is already listening on {}",
                    path.display()
                ));
            }
            Err(_) => {
                log::warn!("removing stale socket {}", path.display());
                std::fs::remove_file(path)
                    .map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
            }
        }
    }
    let listener = UnixListener::bind(path)
        .map_err(|error| format!("cannot bind {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    log::info!("IPC listening on {}", path.display());
    Ok(listener)
}

/// Accept connections forever.
pub async fn serve(listener: UnixListener, tx: mpsc::UnboundedSender<Message>) {
    loop {
        match listener.accept().await {
            Ok((stream, _address)) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle(stream, tx).await {
                        log::debug!("IPC connection closed: {error}");
                    }
                });
            }
            Err(error) => {
                log::warn!("IPC accept failed: {error}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}

async fn handle(stream: UnixStream, tx: mpsc::UnboundedSender<Message>) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let (response_tx, response_rx) = oneshot::channel();
                if tx.send(Message::Ipc(request, response_tx)).is_err() {
                    Response::Error("cosmic-mac-wm is shutting down".into())
                } else {
                    response_rx
                        .await
                        .unwrap_or_else(|_| Response::Error("layout owner stopped".into()))
                }
            }
            Err(error) => Response::Error(format!("invalid request: {error}")),
        };
        let mut text = match serde_json::to_string(&response) {
            Ok(text) => text,
            Err(error) => {
                format!(r#"{{"response":"error","data":"serialize failed: {error}"}}"#)
            }
        };
        text.push('\n');
        writer.write_all(text.as_bytes()).await?;
        writer.flush().await?;
    }
    Ok(())
}
