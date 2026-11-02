// SPDX-License-Identifier: GPL-3.0-or-later
//! `Request::Subscribe` end to end: the real daemon loop (`daemon::run`) on a
//! private socket, with an in-memory platform backend.  Checks that the loop
//! publishes the shell state after startup and after every message, and only
//! when it changed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cosmic_mac_protocol::{
    Action, Config, Rect, Request, Response, ShellState, StackInfo, ToplevelInfo, WindowId,
    WorkspaceInfo,
};
use cosmic_mac_wm::backend::{AppliedFrame, PlatformBackend, PlatformResult, SizeConstraints};
use cosmic_mac_wm::daemon::{self, Launch, Message};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

const WORK_AREA: Rect = Rect::new(0.0, 0.0, 1000.0, 800.0);
const QUIET: Duration = Duration::from_millis(200);
const PATIENCE: Duration = Duration::from_secs(5);
/// Not the test process, which the daemon never manages.
const APP_PID: i32 = 4242;

/// Windows that accept every frame, with park/unpark bookkeeping.
#[derive(Default)]
struct Backend {
    frames: Mutex<HashMap<WindowId, Rect>>,
    parked: Mutex<HashMap<WindowId, Rect>>,
}

impl Backend {
    fn with_windows(wids: &[WindowId]) -> Self {
        let backend = Backend::default();
        for (index, wid) in wids.iter().enumerate() {
            let offset = 50.0 * index as f64;
            backend
                .frames
                .lock()
                .unwrap()
                .insert(*wid, Rect::new(offset, offset, 400.0, 300.0));
        }
        backend
    }

    fn info(wid: WindowId, frame: Rect) -> ToplevelInfo {
        let mut info = ToplevelInfo::new(wid, "com.example.app", format!("window {wid}"))
            .with_pid(APP_PID)
            .with_initial_frame(frame);
        info.role = Some("AXWindow".into());
        info
    }
}

impl PlatformBackend for Backend {
    fn set_frame(&self, wid: WindowId, rect: Rect) -> PlatformResult<AppliedFrame> {
        self.frames.lock().unwrap().insert(wid, rect);
        Ok(AppliedFrame::new(rect, rect))
    }
    fn size_constraints(&self, _wid: WindowId) -> PlatformResult<Option<SizeConstraints>> {
        Ok(None)
    }
    fn raise(&self, _wid: WindowId) -> PlatformResult<()> {
        Ok(())
    }
    fn focus(&self, _wid: WindowId) -> PlatformResult<()> {
        Ok(())
    }
    fn close(&self, _wid: WindowId) -> PlatformResult<()> {
        Ok(())
    }
    fn park(&self, wid: WindowId, _display_frame: Rect) -> PlatformResult<Option<Rect>> {
        let frame = self.frame(wid).unwrap_or(Rect::ZERO);
        self.parked.lock().unwrap().entry(wid).or_insert(frame);
        Ok(Some(frame))
    }
    fn unpark(&self, wid: WindowId) -> PlatformResult<Option<Rect>> {
        Ok(self.parked.lock().unwrap().remove(&wid))
    }
    fn saved_frame(&self, wid: WindowId) -> Option<Rect> {
        self.parked.lock().unwrap().get(&wid).copied()
    }
    fn parked(&self) -> Vec<(WindowId, Rect)> {
        self.parked
            .lock()
            .unwrap()
            .iter()
            .map(|(&wid, &frame)| (wid, frame))
            .collect()
    }
    fn forget(&self, wid: WindowId) {
        self.parked.lock().unwrap().remove(&wid);
    }
    fn restore_all(&self) -> Vec<(WindowId, String)> {
        let parked: Vec<_> = self.parked.lock().unwrap().drain().collect();
        let mut frames = self.frames.lock().unwrap();
        for (wid, frame) in parked {
            frames.insert(wid, frame);
        }
        Vec::new()
    }
    fn frame(&self, wid: WindowId) -> Option<Rect> {
        self.frames.lock().unwrap().get(&wid).copied()
    }
    fn window_info(&self, wid: WindowId) -> Option<ToplevelInfo> {
        self.frame(wid).map(|frame| Backend::info(wid, frame))
    }
    fn windows(&self) -> Vec<ToplevelInfo> {
        let mut windows: Vec<_> = self
            .frames
            .lock()
            .unwrap()
            .iter()
            .map(|(&wid, &frame)| Backend::info(wid, frame))
            .collect();
        windows.sort_by_key(|info| info.wid);
        windows
    }
    fn set_stack_bars(&self, _stacks: Vec<StackInfo>) -> PlatformResult<()> {
        Ok(())
    }
    fn main_work_area(&self) -> Rect {
        WORK_AREA
    }
    fn display_frame(&self, _wid: WindowId) -> Rect {
        WORK_AREA
    }
}

struct Daemon {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    tx: mpsc::UnboundedSender<Message>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

impl Daemon {
    async fn start(windows: &[WindowId]) -> Self {
        // Unix socket paths are limited to ~104 bytes; keep the directory short.
        let dir = tempfile::Builder::new()
            .prefix("cmw-sub")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("wm.sock");
        let launch = Launch {
            backend: Arc::new(Backend::with_windows(windows)),
            config: Config::default(),
            work_area: WORK_AREA,
            journal_path: dir.path().join("journal.ron"),
            session_path: dir.path().join("session.ron"),
            socket_path: socket.clone(),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(daemon::run(launch, rx, tx.clone()));
        let daemon = Daemon {
            _dir: dir,
            socket,
            tx,
            task,
        };
        // A one-shot query is answered by the loop, so once it returns the
        // startup layout has been published.
        tokio::time::timeout(PATIENCE, async {
            loop {
                if let Ok(mut connection) = UnixStream::connect(&daemon.socket).await.map(split) {
                    connection.send(&Request::ListWorkspaces).await;
                    if matches!(connection.next().await, Response::Workspaces(_)) {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the daemon starts listening");
        daemon
    }

    async fn connect(&self) -> Connection {
        split(UnixStream::connect(&self.socket).await.unwrap())
    }

    async fn subscribe(&self) -> Connection {
        let mut connection = self.connect().await;
        connection.send(&Request::Subscribe).await;
        connection
    }

    /// Send one request on its own connection, like `cosmic-mac-ctl`.
    async fn request(&self, request: Request) -> Response {
        let mut connection = self.connect().await;
        connection.send(&request).await;
        connection.next().await
    }

    async fn shutdown(self) {
        self.tx.send(Message::Shutdown).unwrap();
        tokio::time::timeout(PATIENCE, self.task)
            .await
            .expect("the daemon stops")
            .unwrap()
            .unwrap();
    }
}

fn split(stream: UnixStream) -> Connection {
    let (reader, writer) = stream.into_split();
    Connection {
        lines: BufReader::new(reader).lines(),
        writer,
    }
}

struct Connection {
    lines: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
}

impl Connection {
    async fn send(&mut self, request: &Request) {
        let mut line = serde_json::to_string(request).unwrap();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn next(&mut self) -> Response {
        let line = tokio::time::timeout(PATIENCE, self.lines.next_line())
            .await
            .expect("a line in time")
            .unwrap()
            .expect("a line, not EOF");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_streams_shell_state_changes_to_subscribers() {
    let daemon = Daemon::start(&[1, 2]).await;
    let mut subscriber = daemon.subscribe().await;
    let initial = subscriber.next_state().await;
    assert_eq!(
        initial.workspaces,
        vec![WorkspaceInfo::new(1, "1", true, 2)]
    );
    assert!(initial.tiling);
    assert_eq!(initial.focused_floating, Some(false));
    assert_eq!(initial.max_workspaces, 9);

    // A second subscriber starts from the same state.
    let mut other = daemon.subscribe().await;
    assert_eq!(other.next_state().await, initial);

    // Switching workspace creates workspace 2 and moves the active flag.
    let reply = daemon
        .request(Request::Action(Action::Workspace { index: 2 }))
        .await;
    assert_eq!(reply, Response::Ok);
    let switched = subscriber.next_state().await;
    assert_eq!(
        switched.workspaces,
        vec![
            WorkspaceInfo::new(1, "1", false, 2),
            WorkspaceInfo::new(2, "2", true, 0),
        ]
    );
    assert_eq!(switched.focused_floating, None);
    assert_eq!(other.next_state().await, switched);

    // Queries change nothing, so subscribers hear nothing.
    assert!(matches!(
        daemon.request(Request::ListWindows).await,
        Response::Windows(_)
    ));
    assert!(matches!(
        daemon.request(Request::Tree).await,
        Response::Tree(_)
    ));
    subscriber.assert_silent().await;

    // One subscriber leaves; the other still hears every change.
    drop(other);
    daemon
        .request(Request::Action(Action::Workspace { index: 1 }))
        .await;
    let back = subscriber.next_state().await;
    assert_eq!(back.active_workspace().map(|info| info.index), Some(1));
    daemon
        .request(Request::Action(Action::ToggleWindowFloating))
        .await;
    assert_eq!(subscriber.next_state().await.focused_floating, Some(true));
    daemon.request(Request::Action(Action::ToggleTiling)).await;
    assert!(!subscriber.next_state().await.tiling);

    // A subscriber that arrives later starts from the current state.
    let mut late = daemon.subscribe().await;
    assert!(!late.next_state().await.tiling);

    daemon.shutdown().await;
    subscriber.assert_closed().await;
    late.assert_closed().await;
}
