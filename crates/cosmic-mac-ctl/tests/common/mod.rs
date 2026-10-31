// SPDX-License-Identifier: GPL-3.0-or-later
//! Mock daemon harness for the CLI integration tests.
//!
//! Each [`MockDaemon`] binds a private Unix socket in a temporary directory, accepts any
//! number of one-shot connections, records every request line and answers with the next
//! canned reply.  This exercises the real binary end to end without the daemon crate.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temporary directory removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Unix socket paths are limited to ~104 bytes, so keep the temporary path short.
        // /tmp is a symlink to /private/tmp on macOS and is always writable by the user.
        let tag: String = prefix
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .take(6)
            .collect();
        let path = PathBuf::from("/tmp").join(format!(
            "cmc-{tag}-{}-{nanos}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A one-shot-per-connection mock server.
pub struct MockDaemon {
    dir: TempDir,
    path: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockDaemon {
    pub fn start(replies: impl IntoIterator<Item = String>) -> Self {
        Self::with_delay(replies, Duration::ZERO)
    }

    pub fn start_with(replies: &[&str]) -> Self {
        Self::start(replies.iter().map(|reply| (*reply).to_string()))
    }

    /// Start a server that waits `delay` before answering each request.
    pub fn with_delay(replies: impl IntoIterator<Item = String>, delay: Duration) -> Self {
        let dir = TempDir::new("cosmic-mac-ctl-mock");
        let path = dir.path().join("cosmic-mac-wm.sock");
        let listener = UnixListener::bind(&path).expect("bind mock socket");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");

        let queue = Arc::new(Mutex::new(VecDeque::from_iter(replies)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_queue = Arc::clone(&queue);
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !thread_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

                        let mut line = String::new();
                        {
                            let mut reader = BufReader::new(&stream);
                            if reader.read_line(&mut line).is_err() {
                                continue;
                            }
                        }
                        if line.is_empty() {
                            continue;
                        }
                        thread_requests
                            .lock()
                            .expect("request lock")
                            .push(line.trim_end().to_string());

                        let reply = thread_queue.lock().expect("reply lock").pop_front();
                        if let Some(reply) = reply {
                            // An empty reply means "close without answering".
                            if !reply.is_empty() {
                                if !delay.is_zero() {
                                    thread::sleep(delay);
                                }
                                let _ = stream.write_all(reply.as_bytes());
                                if !reply.ends_with('\n') {
                                    let _ = stream.write_all(b"\n");
                                }
                                let _ = stream.flush();
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });

        MockDaemon {
            dir,
            path,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request lock").clone()
    }

    pub fn request_values(&self) -> Vec<serde_json::Value> {
        self.requests()
            .iter()
            .map(|line| serde_json::from_str(line).expect("mock request is valid JSON"))
            .collect()
    }
}

impl Drop for MockDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A CLI command with the environment variables that could leak in from the host removed.
pub fn cli() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cosmic-mac-ctl"));
    command
        .env_remove("COSMIC_MAC_WM_SOCKET")
        .env_remove("COSMIC_MAC_WM_TIMEOUT_MS");
    command
}

/// Run the CLI against `daemon` using `--socket`.
pub fn run(daemon: &MockDaemon, args: &[&str]) -> Output {
    let mut command = cli();
    command.arg("--socket").arg(daemon.path()).args(args);
    command.output().expect("run cosmic-mac-ctl")
}

/// Run the CLI against `daemon` using `COSMIC_MAC_WM_SOCKET` only.
pub fn run_env(daemon: &MockDaemon, args: &[&str]) -> Output {
    let mut command = cli();
    command
        .env("COSMIC_MAC_WM_SOCKET", daemon.path())
        .args(args);
    command.output().expect("run cosmic-mac-ctl")
}

/// Run the CLI against an arbitrary socket path (which may not exist).
pub fn run_at(path: &Path, args: &[&str]) -> Output {
    let mut command = cli();
    command.arg("--socket").arg(path).args(args);
    command.output().expect("run cosmic-mac-ctl")
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("process exited normally")
}

// ---------------------------------------------------------------------------------------------
// Canned payloads
// ---------------------------------------------------------------------------------------------

pub fn windows_payload() -> serde_json::Value {
    serde_json::json!([
        {
            "info": {
                "wid": 101,
                "pid": 900,
                "app_id": "com.apple.TextEdit",
                "title": "notes.txt",
                "role": "AXWindow",
                "subrole": "AXStandardWindow",
                "resizable": true,
                "is_dialog": false,
                "initial_frame": {"x": 20.0, "y": 30.0, "width": 700.0, "height": 500.0}
            },
            "workspace": 1,
            "floating": false,
            "visible": true,
            "focused": true,
            "in_stack": false,
            "stack_active": false,
            "frame": {"x": 0.0, "y": 0.0, "width": 800.0, "height": 600.0}
        },
        {
            "info": {
                "wid": 202,
                "pid": 901,
                "app_id": "com.apple.Terminal",
                "title": "bash",
                "role": "AXWindow",
                "subrole": "AXStandardWindow",
                "resizable": true,
                "is_dialog": false,
                "initial_frame": null
            },
            "workspace": 2,
            "floating": true,
            "visible": false,
            "focused": false,
            "in_stack": true,
            "stack_active": false,
            "frame": {"x": 800.0, "y": 0.0, "width": 800.0, "height": 600.0}
        }
    ])
}

pub fn workspaces_payload() -> serde_json::Value {
    serde_json::json!([
        {"index": 1, "name": "1", "active": true, "window_count": 2},
        {"index": 2, "name": "2", "active": false, "window_count": 0}
    ])
}

pub fn tree_payload() -> serde_json::Value {
    serde_json::json!({
        "kind": "group",
        "orientation": "horizontal",
        "frame": {"x": 0.0, "y": 0.0, "width": 1600.0, "height": 900.0},
        "children": [
            {
                "kind": "window",
                "wid": 101,
                "title": "notes.txt",
                "frame": {"x": 0.0, "y": 0.0, "width": 800.0, "height": 900.0}
            },
            {
                "kind": "stack",
                "frame": {"x": 800.0, "y": 0.0, "width": 800.0, "height": 900.0},
                "active": 0,
                "tabs": [
                    {"wid": 202, "title": "bash"},
                    {"wid": 203, "title": "zsh"}
                ]
            }
        ]
    })
}

pub fn config_payload() -> serde_json::Value {
    serde_json::json!({
        "gaps": {"outer": 8, "inner": 4},
        "active_hint": 2,
        "autotile": true,
        "tiling_exceptions": [
            {"app_id": "com.apple.finder", "title": null}
        ],
        "passthrough": ["com.apple.Terminal"]
    })
}

pub fn reply(response: &str, data: serde_json::Value) -> String {
    serde_json::json!({"response": response, "data": data}).to_string()
}

pub fn ok_reply() -> String {
    serde_json::json!({"response": "ok"}).to_string()
}

pub fn error_reply(message: &str) -> String {
    serde_json::json!({"response": "error", "data": message}).to_string()
}

pub fn windows_reply() -> String {
    reply("windows", windows_payload())
}

pub fn workspaces_reply() -> String {
    reply("workspaces", workspaces_payload())
}

pub fn tree_reply() -> String {
    reply("tree", tree_payload())
}

pub fn config_reply() -> String {
    reply("config", config_payload())
}
