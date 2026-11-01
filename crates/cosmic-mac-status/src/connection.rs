// SPDX-License-Identifier: GPL-3.0-or-later
//! The two background threads that talk to the window manager.
//!
//! * [`run_subscription`] owns the `Request::Subscribe` connection: it reports
//!   every streamed state, reports [`Connection::Disconnected`] when the
//!   connection fails or ends, and reconnects with [`Backoff`].
//! * [`spawn_action_worker`] sends the menu's one-shot requests, each on its
//!   own connection, so a slow window manager never blocks the main thread.
//!
//! Neither thread touches AppKit; `crate::appkit` marshals reports onto the
//! main thread.

use std::ops::ControlFlow;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cosmic_mac_ctl::client::{Client, Reply};
use cosmic_mac_protocol::{Request, Response};

use crate::model::Connection;

/// Reconnect delays: doubling from `initial` up to `max`, back to `initial`
/// once a subscription delivers a state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    /// First retry after a failure.
    pub const INITIAL: Duration = Duration::from_millis(250);
    /// Longest wait between attempts.  Connecting to a Unix socket is cheap,
    /// and launchd restarts a crashed window manager within a few seconds.
    pub const MAX: Duration = Duration::from_secs(2);

    pub fn new(initial: Duration, max: Duration) -> Self {
        let initial = initial.min(max);
        Backoff {
            initial,
            max,
            next: initial,
        }
    }

    /// The delay before the next attempt; each call doubles the following one.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.max);
        delay
    }

    /// Start over from the initial delay.
    pub fn reset(&mut self) {
        self.next = self.initial;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::new(Backoff::INITIAL, Backoff::MAX)
    }
}

/// Logs the first failure of a run of failures at `info` and the rest at
/// `debug`, so a window manager that is down does not fill the log.
#[derive(Debug, Default)]
struct FailureLog {
    quiet: bool,
}

impl FailureLog {
    fn failed(&mut self, message: std::fmt::Arguments<'_>) {
        if self.quiet {
            log::debug!("{message}");
        } else {
            log::info!("{message}; retrying in the background");
            self.quiet = true;
        }
    }

    fn succeeded(&mut self) {
        self.quiet = false;
    }
}

/// Subscribe to the window manager and report what happens to `report`, until
/// it returns [`ControlFlow::Break`].
///
/// Reports [`Connection::Connected`] for every streamed state, and
/// [`Connection::Disconnected`] once each time the connection is lost (or the
/// first attempt fails), then sleeps per `backoff` and reconnects.  A window
/// manager that predates subscriptions answers with an error; that counts as
/// a failed attempt.
pub fn run_subscription(
    client: &Client,
    mut backoff: Backoff,
    mut report: impl FnMut(Connection) -> ControlFlow<()>,
) {
    let mut failures = FailureLog::default();
    let mut reported_connected = None;
    loop {
        match client.subscribe() {
            Ok(mut subscription) => loop {
                match subscription.next_reply() {
                    Ok(Some(Reply {
                        response: Response::State(state),
                        ..
                    })) => {
                        if reported_connected != Some(true) {
                            log::info!("subscribed to {}", client.path().display());
                        }
                        backoff.reset();
                        failures.succeeded();
                        reported_connected = Some(true);
                        if report(Connection::Connected(state)).is_break() {
                            return;
                        }
                    }
                    Ok(Some(Reply {
                        response: Response::Error(message),
                        ..
                    })) => {
                        failures.failed(format_args!(
                            "the window manager refused the subscription: {message}"
                        ));
                        break;
                    }
                    Ok(Some(Reply { response, .. })) => {
                        failures.failed(format_args!(
                            "unexpected reply on the subscription: {response:?}"
                        ));
                        break;
                    }
                    Ok(None) => {
                        failures.failed(format_args!("the window manager closed the subscription"));
                        break;
                    }
                    Err(error) => {
                        failures.failed(format_args!("subscription failed: {error}"));
                        break;
                    }
                }
            },
            Err(error) => {
                failures.failed(format_args!("cannot subscribe: {error}"));
            }
        }
        if reported_connected != Some(false) {
            reported_connected = Some(false);
            if report(Connection::Disconnected).is_break() {
                return;
            }
        }
        thread::sleep(backoff.next_delay());
    }
}

/// Send one menu request and check the reply.
pub fn send(client: &Client, request: &Request) -> Result<(), String> {
    match client.request(request) {
        Ok(Reply {
            response: Response::Ok,
            ..
        }) => Ok(()),
        Ok(Reply {
            response: Response::Error(message),
            ..
        }) => Err(format!("the window manager refused it: {message}")),
        Ok(Reply { response, .. }) => Err(format!("unexpected reply {response:?}")),
        Err(error) => Err(error.to_string()),
    }
}

/// Start the thread that sends menu requests, in order.  It ends when the
/// returned sender is dropped.
pub fn spawn_action_worker(client: Client) -> std::io::Result<mpsc::Sender<Request>> {
    let (tx, rx) = mpsc::channel::<Request>();
    thread::Builder::new()
        .name("cosmic-mac-status-actions".into())
        .spawn(move || {
            for request in rx {
                match send(&client, &request) {
                    Ok(()) => log::debug!("sent {request:?}"),
                    Err(error) => log::warn!("{request:?} failed: {error}"),
                }
            }
        })?;
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::time::Instant;

    use cosmic_mac_protocol::{Action, ShellState, WorkspaceInfo};

    const PATIENCE: Duration = Duration::from_secs(5);

    fn fast() -> Backoff {
        Backoff::new(Duration::from_millis(10), Duration::from_millis(40))
    }

    fn state(active: u32) -> ShellState {
        ShellState {
            workspaces: (1..=2)
                .map(|index| WorkspaceInfo::new(index, index.to_string(), index == active, 1))
                .collect(),
            tiling: true,
            focused_floating: None,
            max_workspaces: 9,
        }
    }

    fn line(response: &Response) -> String {
        format!("{}\n", serde_json::to_string(response).unwrap())
    }

    /// A socket path in a fresh temporary directory.
    fn socket() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("cms")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("wm.sock");
        (dir, path)
    }

    /// Accept connections on `path`; the n-th one is answered with
    /// `sessions[n]` (each line written in turn, then closed), and every
    /// request line is sent to the returned channel.
    fn serve(path: &PathBuf, sessions: Vec<Vec<String>>) -> mpsc::Receiver<String> {
        let listener = UnixListener::bind(path).unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for lines in sessions {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                let _ = tx.send(request.trim_end().to_string());
                for line in lines {
                    stream.write_all(line.as_bytes()).unwrap();
                }
            }
        });
        rx
    }

    #[test]
    fn backoff_doubles_up_to_the_cap_and_resets() {
        let mut backoff = Backoff::new(Duration::from_millis(250), Duration::from_secs(2));
        let delays: Vec<u128> = (0..6).map(|_| backoff.next_delay().as_millis()).collect();
        assert_eq!(delays, [250, 500, 1000, 2000, 2000, 2000]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(250));
        assert_eq!(
            Backoff::default(),
            Backoff::new(Backoff::INITIAL, Backoff::MAX)
        );
        // An initial delay above the cap is clamped.
        let mut clamped = Backoff::new(Duration::from_secs(9), Duration::from_secs(1));
        assert_eq!(clamped.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn reports_states_then_disconnect_then_reconnects() {
        let (_dir, path) = socket();
        let requests = serve(
            &path,
            vec![
                vec![
                    line(&Response::State(state(1))),
                    line(&Response::State(state(2))),
                ],
                vec![line(&Response::State(state(1)))],
            ],
        );
        let client = Client::new(path, PATIENCE);
        let mut seen = Vec::new();
        run_subscription(&client, fast(), |connection| {
            seen.push(connection);
            if seen.len() == 4 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert_eq!(
            seen,
            [
                Connection::Connected(state(1)),
                Connection::Connected(state(2)),
                Connection::Disconnected,
                Connection::Connected(state(1)),
            ]
        );
        let subscribe = r#"{"request":"subscribe"}"#;
        assert_eq!(requests.recv().unwrap(), subscribe);
        assert_eq!(requests.recv().unwrap(), subscribe);
    }

    #[test]
    fn waits_for_a_window_manager_that_starts_later() {
        let (_dir, path) = socket();
        let client = Client::new(path.clone(), PATIENCE);
        // Several attempts fail (backoff 10, 20, 40, 40 … ms) before it is up.
        let late = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            serve(&path, vec![vec![line(&Response::State(state(2)))]])
        });
        let started = Instant::now();
        let mut seen = Vec::new();
        run_subscription(&client, fast(), |connection| {
            let connected = matches!(connection, Connection::Connected(_));
            seen.push(connection);
            if connected || started.elapsed() > PATIENCE {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        // Disconnected is reported once for the outage, not once per attempt.
        assert_eq!(
            seen,
            [Connection::Disconnected, Connection::Connected(state(2))]
        );
        assert!(started.elapsed() >= Duration::from_millis(150));
        let requests = late.join().unwrap();
        assert_eq!(
            requests.recv_timeout(PATIENCE).unwrap(),
            r#"{"request":"subscribe"}"#
        );
    }

    #[test]
    fn a_refused_subscription_counts_as_disconnected() {
        let (_dir, path) = socket();
        let refused = line(&Response::Error(
            "invalid request: unknown variant `subscribe`".into(),
        ));
        let _requests = serve(
            &path,
            vec![vec![refused], vec![line(&Response::State(state(1)))]],
        );
        let client = Client::new(path, PATIENCE);
        let mut seen = Vec::new();
        run_subscription(&client, fast(), |connection| {
            seen.push(connection);
            if seen.len() == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert_eq!(
            seen,
            [Connection::Disconnected, Connection::Connected(state(1))]
        );
    }

    #[test]
    fn send_reports_ok_errors_and_unreachable() {
        let (_dir, path) = socket();
        let requests = serve(
            &path,
            vec![
                vec![line(&Response::Ok)],
                vec![line(&Response::Error("no focused window".into()))],
            ],
        );
        let client = Client::new(path.clone(), PATIENCE);
        let request = Request::Action(Action::Workspace { index: 3 });
        assert_eq!(send(&client, &request), Ok(()));
        assert_eq!(
            requests.recv().unwrap(),
            r#"{"request":"action","data":{"action":"workspace","args":{"index":3}}}"#
        );
        let error = send(&client, &Request::Action(Action::ToggleWindowFloating)).unwrap_err();
        assert!(error.contains("no focused window"), "{error}");

        let absent = Client::new(path.with_file_name("absent.sock"), PATIENCE);
        let error = send(&absent, &request).unwrap_err();
        assert!(error.contains("daemon not running"), "{error}");
    }

    #[test]
    fn the_action_worker_sends_requests_in_order() {
        let (_dir, path) = socket();
        let requests = serve(
            &path,
            vec![vec![line(&Response::Ok)], vec![line(&Response::Ok)]],
        );
        let worker = spawn_action_worker(Client::new(path, PATIENCE)).unwrap();
        worker.send(Request::Action(Action::ToggleTiling)).unwrap();
        worker
            .send(Request::Action(Action::Workspace { index: 2 }))
            .unwrap();
        assert_eq!(
            requests.recv_timeout(PATIENCE).unwrap(),
            r#"{"request":"action","data":{"action":"toggle_tiling"}}"#
        );
        assert_eq!(
            requests.recv_timeout(PATIENCE).unwrap(),
            r#"{"request":"action","data":{"action":"workspace","args":{"index":2}}}"#
        );
    }
}
