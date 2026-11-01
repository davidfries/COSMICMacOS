// SPDX-License-Identifier: GPL-3.0-or-later
//! Tokio Unix-socket IPC server.
//!
//! One JSON [`Request`] per line, one JSON [`Response`] per line.  Requests are
//! forwarded to the single-threaded layout owner and the reply is awaited on a
//! oneshot channel, so queries always see a consistent snapshot.
//!
//! [`Request::Subscribe`] is answered here instead: the connection becomes a
//! stream of [`Response::State`] lines fed by the daemon's
//! [`shell_state::Publisher`](crate::shell_state::Publisher), the current state
//! first and then one line per change, until the client disconnects or the
//! daemon stops.

use std::path::Path;

use cosmic_mac_protocol::{Request, Response, ShellState};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};

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

/// Accept connections forever.  `shell_state` feeds subscriptions; each
/// connection gets its own receiver.
pub async fn serve(
    listener: UnixListener,
    tx: mpsc::UnboundedSender<Message>,
    shell_state: watch::Receiver<ShellState>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _address)) => {
                let tx = tx.clone();
                let shell_state = shell_state.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle(stream, tx, shell_state).await {
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

async fn handle(
    stream: UnixStream,
    tx: mpsc::UnboundedSender<Message>,
    shell_state: watch::Receiver<ShellState>,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Subscribe) => return subscription(lines, writer, shell_state).await,
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
        write_response(&mut writer, &response).await?;
    }
    Ok(())
}

/// Serve a subscribed connection: the current shell state at once, then every
/// change.  Ends when the client closes the connection (or a write fails) or
/// the daemon drops its publisher.  Lines the client sends are read only to
/// notice the disconnect and are otherwise ignored.
async fn subscription(
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut writer: OwnedWriteHalf,
    mut shell_state: watch::Receiver<ShellState>,
) -> std::io::Result<()> {
    log::debug!("IPC subscriber connected");
    let current = shell_state.borrow_and_update().clone();
    write_response(&mut writer, &Response::State(current)).await?;
    loop {
        // Both branches are cancel safe: an unfinished line stays buffered in
        // `lines`, and a missed change is still pending on `shell_state`.
        tokio::select! {
            changed = shell_state.changed() => {
                if changed.is_err() {
                    // The daemon dropped its publisher: it is shutting down.
                    return Ok(());
                }
                let current = shell_state.borrow_and_update().clone();
                write_response(&mut writer, &Response::State(current)).await?;
            }
            line = lines.next_line() => {
                if line?.is_none() {
                    log::debug!("IPC subscriber disconnected");
                    return Ok(());
                }
            }
        }
    }
}

async fn write_response(writer: &mut OwnedWriteHalf, response: &Response) -> std::io::Result<()> {
    let mut text = match serde_json::to_string(response) {
        Ok(text) => text,
        Err(error) => {
            format!(r#"{{"response":"error","data":"serialize failed: {error}"}}"#)
        }
    };
    text.push('\n');
    writer.write_all(text.as_bytes()).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use cosmic_mac_protocol::{Action, WorkspaceInfo};

    use crate::shell_state::Publisher;

    /// How long a test waits to conclude that nothing was sent.
    const QUIET: Duration = Duration::from_millis(150);
    /// Upper bound for anything that should happen promptly.
    const PATIENCE: Duration = Duration::from_secs(5);

    fn state(active: u32, workspaces: u32) -> ShellState {
        ShellState {
            workspaces: (1..=workspaces)
                .map(|index| WorkspaceInfo::new(index, index.to_string(), index == active, 1))
                .collect(),
            tiling: true,
            focused_floating: Some(false),
            max_workspaces: 9,
        }
    }

    /// The IPC server on a private socket, with a stand-in for the core that
    /// records every request it is asked and answers queries and actions.
    struct Server {
        _dir: tempfile::TempDir,
        path: PathBuf,
        publisher: Publisher,
        core_requests: Arc<Mutex<Vec<Request>>>,
    }

    impl Server {
        fn start() -> Self {
            // Unix socket paths are limited to ~104 bytes; keep the directory short.
            let dir = tempfile::Builder::new()
                .prefix("cmw-ipc")
                .tempdir_in("/tmp")
                .unwrap();
            let path = dir.path().join("wm.sock");
            let listener = bind(&path).unwrap();
            let publisher = Publisher::with_state(state(1, 1));
            let core_requests = Arc::new(Mutex::new(Vec::new()));
            let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
            let recorded = Arc::clone(&core_requests);
            tokio::spawn(async move {
                while let Some(message) = rx.recv().await {
                    if let Message::Ipc(request, reply) = message {
                        let response = match &request {
                            Request::ListWorkspaces => {
                                Response::Workspaces(vec![WorkspaceInfo::new(1, "1", true, 1)])
                            }
                            Request::Action(_) => Response::Ok,
                            _ => Response::Error("not in this test".into()),
                        };
                        recorded.lock().unwrap().push(request);
                        let _ = reply.send(response);
                    }
                }
            });
            tokio::spawn(serve(listener, tx, publisher.subscribe()));
            Server {
                _dir: dir,
                path,
                publisher,
                core_requests,
            }
        }

        async fn connect(&self) -> Connection {
            let stream = UnixStream::connect(&self.path).await.unwrap();
            let (reader, writer) = stream.into_split();
            Connection {
                lines: BufReader::new(reader).lines(),
                writer,
            }
        }

        async fn subscribe(&self) -> Connection {
            let mut connection = self.connect().await;
            connection.send(&Request::Subscribe).await;
            connection
        }

        /// Wait until exactly `count` receivers are alive (the server's own plus
        /// one per open connection).
        async fn wait_for_receivers(&self, count: usize) {
            tokio::time::timeout(PATIENCE, async {
                while self.publisher.receiver_count() != count {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "expected {count} receivers, have {}",
                    self.publisher.receiver_count()
                )
            });
        }
    }

    struct Connection {
        lines: Lines<BufReader<OwnedReadHalf>>,
        writer: OwnedWriteHalf,
    }

    impl Connection {
        async fn send(&mut self, request: &Request) {
            let line = serde_json::to_string(request).unwrap();
            self.send_raw(&line).await;
        }

        async fn send_raw(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).await.unwrap();
            self.writer.write_all(b"\n").await.unwrap();
            self.writer.flush().await.unwrap();
        }

        async fn next(&mut self) -> Response {
            let line = tokio::time::timeout(PATIENCE, self.lines.next_line())
                .await
                .expect("a reply in time")
                .unwrap()
                .expect("a reply, not EOF");
            serde_json::from_str(&line).unwrap()
        }

        async fn next_state(&mut self) -> ShellState {
            match self.next().await {
                Response::State(state) => state,
                other => panic!("expected a state, got {other:?}"),
            }
        }

        async fn assert_silent(&mut self) {
            if let Ok(line) = tokio::time::timeout(QUIET, self.lines.next_line()).await {
                panic!("expected no line, got {line:?}");
            }
        }

        async fn assert_closed(&mut self) {
            let line = tokio::time::timeout(PATIENCE, self.lines.next_line())
                .await
                .expect("EOF in time")
                .unwrap();
            assert_eq!(line, None);
        }
    }

    #[tokio::test]
    async fn subscribe_sends_the_current_state_then_each_change() {
        let server = Server::start();
        // A change before the subscriber connects is its starting point.
        server.publisher.publish(state(1, 2));
        let mut subscriber = server.subscribe().await;
        assert_eq!(subscriber.next_state().await, state(1, 2));

        server.publisher.publish(state(2, 2));
        assert_eq!(subscriber.next_state().await, state(2, 2));
        server.publisher.publish(state(2, 3));
        assert_eq!(subscriber.next_state().await, state(2, 3));
        // Subscriptions are answered by the IPC server, never by the core.
        assert!(server.core_requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unchanged_state_sends_nothing() {
        let server = Server::start();
        let mut subscriber = server.subscribe().await;
        assert_eq!(subscriber.next_state().await, state(1, 1));

        assert!(!server.publisher.publish(state(1, 1)));
        subscriber.assert_silent().await;

        // The next real change is the very next line.
        assert!(server.publisher.publish(state(1, 2)));
        assert_eq!(subscriber.next_state().await, state(1, 2));
    }

    #[tokio::test]
    async fn a_disconnecting_subscriber_does_not_affect_the_others() {
        let server = Server::start();
        let mut first = server.subscribe().await;
        let mut second = server.subscribe().await;
        first.next_state().await;
        second.next_state().await;
        server.wait_for_receivers(3).await;

        drop(first);
        // Its connection task notices the disconnect and ends.
        server.wait_for_receivers(2).await;

        server.publisher.publish(state(2, 2));
        assert_eq!(second.next_state().await, state(2, 2));
        let mut third = server.subscribe().await;
        assert_eq!(third.next_state().await, state(2, 2));
        server.publisher.publish(state(1, 2));
        assert_eq!(second.next_state().await, state(1, 2));
        assert_eq!(third.next_state().await, state(1, 2));
    }

    #[tokio::test]
    async fn one_shot_requests_keep_working_next_to_a_subscription() {
        let server = Server::start();
        let mut subscriber = server.subscribe().await;
        subscriber.next_state().await;

        // The old framing: several requests on one connection, one reply each.
        let mut client = server.connect().await;
        client.send(&Request::ListWorkspaces).await;
        assert_eq!(
            client.next().await,
            Response::Workspaces(vec![WorkspaceInfo::new(1, "1", true, 1)])
        );
        client
            .send(&Request::Action(Action::Workspace { index: 2 }))
            .await;
        assert_eq!(client.next().await, Response::Ok);
        client.send_raw("not json").await;
        assert!(
            matches!(client.next().await, Response::Error(message) if message.starts_with("invalid request"))
        );

        assert_eq!(
            *server.core_requests.lock().unwrap(),
            vec![
                Request::ListWorkspaces,
                Request::Action(Action::Workspace { index: 2 }),
            ]
        );
        subscriber.assert_silent().await;
    }

    #[tokio::test]
    async fn input_on_a_subscribed_connection_is_ignored() {
        let server = Server::start();
        let mut subscriber = server.subscribe().await;
        subscriber.next_state().await;

        subscriber.send(&Request::ListWorkspaces).await;
        subscriber.send_raw("not json").await;
        subscriber.assert_silent().await;
        assert!(server.core_requests.lock().unwrap().is_empty());

        server.publisher.publish(state(2, 2));
        assert_eq!(subscriber.next_state().await, state(2, 2));
    }

    #[tokio::test]
    async fn dropping_the_publisher_ends_every_subscription() {
        let server = Server::start();
        let mut subscriber = server.subscribe().await;
        subscriber.next_state().await;

        let Server {
            _dir,
            path: _,
            publisher,
            core_requests: _,
        } = server;
        // The daemon drops its publisher when the loop ends (shutdown).
        drop(publisher);
        subscriber.assert_closed().await;
    }
}
