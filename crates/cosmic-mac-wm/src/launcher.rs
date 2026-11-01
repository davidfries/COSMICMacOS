// SPDX-License-Identifier: GPL-3.0-or-later
//! The window manager's client of the launcher's control socket
//! (`cosmic_mac_protocol::launcher`): a Super tap sends `toggle` to the
//! resident launcher, and starts the launcher on demand when nothing
//! answers.
//!
//! The event tap must never block, so [`LauncherClient::toggle`] only queues
//! a request for the `launcher-client` thread, which connects, writes and,
//! on failure, spawns the launcher:
//!
//! 1. `open -a <bin-dir>/cosmic-mac-launcher.app --args --show`, so
//!    LaunchServices starts the bundle (bundle id, `LSUIElement`, its own
//!    process group, not a child of the WM);
//! 2. else `<bin-dir>/cosmic-mac-launcher --show` directly.
//!
//! `<bin-dir>` is the directory of the running `cosmic-mac-wm`. A launcher
//! needs about a second to start listening, so for [`SPAWN_GRACE`] after a
//! spawn further taps that find no socket do not spawn again; the starting
//! launcher shows itself.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cosmic_mac_protocol::launcher::{self as protocol, LauncherCommand};

/// Write and reply timeout of one request.
pub const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// After a spawn, taps that find no socket within this time do not spawn
/// again.
pub const SPAWN_GRACE: Duration = Duration::from_secs(5);

/// Queued toggles older than this are dropped: the user has long stopped
/// waiting for them.
pub const MAX_TOGGLE_AGE: Duration = Duration::from_secs(1);

/// The launcher bundle's directory name inside the bin dir.
pub const BUNDLE_NAME: &str = "cosmic-mac-launcher.app";
/// The launcher's executable name (in the bin dir and inside the bundle).
pub const BINARY_NAME: &str = "cosmic-mac-launcher";

/// Sends one command and waits up to `timeout` for the reply. A launcher
/// that accepted the command but sent no reply in time still counts as
/// reached (the command was delivered).
pub fn send(socket: &Path, command: LauncherCommand, timeout: Duration) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    stream
        .set_write_timeout(Some(timeout))
        .and_then(|()| stream.set_read_timeout(Some(timeout)))
        .map_err(|error| format!("cannot set socket timeouts: {error}"))?;
    stream
        .write_all(command.line().as_bytes())
        .map_err(|error| format!("cannot send {command}: {error}"))?;
    let mut reply = String::new();
    match BufReader::new(stream).read_line(&mut reply) {
        Ok(0) => {
            log::debug!("launcher closed the connection without replying to {command}");
            Ok(())
        }
        Ok(_) => protocol::parse_reply(&reply).map_err(|reason| format!("launcher: {reason}")),
        Err(error) => {
            log::debug!("no reply from the launcher to {command}: {error}");
            Ok(())
        }
    }
}

/// How to start the launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Whether `program` is `open` (it exits once LaunchServices started
    /// the app, so it is waited for) rather than the launcher itself.
    pub via_open: bool,
}

/// The ways to start the launcher from `bin_dir`, preferred first: the
/// bundle through `open` (with the app's stdout and stderr appended to
/// `log`), then the plain binary. Only existing targets are listed.
pub fn spawn_plans(
    bin_dir: &Path,
    log: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Vec<SpawnPlan> {
    let mut plans = Vec::new();
    let bundle = bin_dir.join(BUNDLE_NAME);
    if exists(&bundle) {
        let mut args: Vec<OsString> = vec!["-a".into(), bundle.into_os_string()];
        if let Some(log) = log {
            for stream in ["--stdout", "--stderr"] {
                args.push(stream.into());
                args.push(log.as_os_str().to_owned());
            }
        }
        args.extend(["--args".into(), "--show".into()]);
        plans.push(SpawnPlan {
            program: PathBuf::from("/usr/bin/open"),
            args,
            via_open: true,
        });
    }
    let binary = bin_dir.join(BINARY_NAME);
    if exists(&binary) {
        plans.push(SpawnPlan {
            program: binary,
            args: vec!["--show".into()],
            via_open: false,
        });
    }
    plans
}

/// Runs `plan`. `open` is waited for and must succeed; the plain binary is
/// started in its own process group (so stopping the WM's launchd job does
/// not take the launcher with it) and reaped by a helper thread.
pub fn run_plan(plan: &SpawnPlan) -> Result<(), String> {
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if plan.via_open {
        let status = command
            .status()
            .map_err(|error| format!("cannot run {}: {error}", plan.program.display()))?;
        return if status.success() {
            Ok(())
        } else {
            Err(format!(
                "{} {:?} failed: {status}",
                plan.program.display(),
                plan.args
            ))
        };
    }
    let mut child = command
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", plan.program.display()))?;
    std::thread::Builder::new()
        .name("launcher-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|error| format!("cannot spawn the reaper thread: {error}"))?;
    Ok(())
}

/// What a tap led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapOutcome {
    /// The resident launcher got the command.
    Sent,
    /// Nothing answered; a launcher was started with `--show`.
    Spawned,
    /// Nothing answered, but a launcher was started moments ago.
    StillStarting,
    /// Nothing answered and nothing could be started.
    Failed(String),
}

/// The side effects behind [`Toggler`], replaceable in tests.
pub trait LauncherPort {
    /// Delivers `command` to a running launcher.
    fn send(&mut self, command: LauncherCommand) -> Result<(), String>;
    /// Starts a launcher that shows itself.
    fn spawn(&mut self) -> Result<(), String>;
}

/// Turns taps into a `toggle` or a spawn, with the spawn grace period.
pub struct Toggler<P> {
    port: P,
    last_spawn: Option<Instant>,
    grace: Duration,
}

impl<P: LauncherPort> Toggler<P> {
    pub fn new(port: P, grace: Duration) -> Self {
        Toggler {
            port,
            last_spawn: None,
            grace,
        }
    }

    /// Handles one tap at `now`.
    pub fn tap(&mut self, now: Instant) -> TapOutcome {
        let unreachable = match self.port.send(LauncherCommand::Toggle) {
            Ok(()) => return TapOutcome::Sent,
            Err(error) => error,
        };
        if self
            .last_spawn
            .is_some_and(|spawned| now.saturating_duration_since(spawned) < self.grace)
        {
            log::debug!("launcher not reachable yet ({unreachable}); it is still starting");
            return TapOutcome::StillStarting;
        }
        log::info!("launcher not reachable ({unreachable}); starting it");
        match self.port.spawn() {
            Ok(()) => {
                self.last_spawn = Some(now);
                TapOutcome::Spawned
            }
            Err(error) => TapOutcome::Failed(format!("{unreachable}; {error}")),
        }
    }

    pub fn port(&self) -> &P {
        &self.port
    }
}

/// The real port: the launcher socket and the bin dir to spawn from.
pub struct SocketPort {
    pub socket: PathBuf,
    pub bin_dir: PathBuf,
    /// Where an on-demand bundle start sends its output.
    pub log: Option<PathBuf>,
}

impl LauncherPort for SocketPort {
    fn send(&mut self, command: LauncherCommand) -> Result<(), String> {
        send(&self.socket, command, IO_TIMEOUT)
    }

    fn spawn(&mut self) -> Result<(), String> {
        if let Some(dir) = self.log.as_deref().and_then(Path::parent) {
            let _ = std::fs::create_dir_all(dir);
        }
        let plans = spawn_plans(&self.bin_dir, self.log.as_deref(), Path::exists);
        if plans.is_empty() {
            return Err(format!(
                "neither {BUNDLE_NAME} nor {BINARY_NAME} is in {}",
                self.bin_dir.display()
            ));
        }
        let mut errors = Vec::new();
        for plan in &plans {
            match run_plan(plan) {
                Ok(()) => {
                    log::info!(
                        "started the launcher: {} {:?}",
                        plan.program.display(),
                        plan.args
                    );
                    return Ok(());
                }
                Err(error) => {
                    log::warn!("{error}");
                    errors.push(error);
                }
            }
        }
        Err(errors.join("; "))
    }
}

/// Reduces the toggles queued while the client thread was busy to the one
/// that matters: stale ones (older than `max_age` at `now`) are dropped, and
/// of the fresh ones an even number cancels out. Returns the time of the
/// last fresh toggle when a single toggle should be sent.
pub fn coalesce_toggles(queued: &[Instant], now: Instant, max_age: Duration) -> Option<Instant> {
    let fresh: Vec<Instant> = queued
        .iter()
        .copied()
        .filter(|at| now.saturating_duration_since(*at) <= max_age)
        .collect();
    if fresh.len() % 2 == 1 {
        fresh.last().copied()
    } else {
        None
    }
}

/// A handle to the `launcher-client` thread.
#[derive(Clone)]
pub struct LauncherClient {
    requests: mpsc::Sender<Instant>,
}

impl LauncherClient {
    /// Starts the client thread over `port`.
    pub fn spawn<P: LauncherPort + Send + 'static>(port: P) -> std::io::Result<Self> {
        let (requests, receiver) = mpsc::channel::<Instant>();
        std::thread::Builder::new()
            .name("launcher-client".into())
            .spawn(move || {
                let mut toggler = Toggler::new(port, SPAWN_GRACE);
                while let Ok(first) = receiver.recv() {
                    // Taps queued while the last request was slow are
                    // dropped or merged, not replayed late.
                    let mut queued = vec![first];
                    queued.extend(receiver.try_iter());
                    let Some(tapped) = coalesce_toggles(&queued, Instant::now(), MAX_TOGGLE_AGE)
                    else {
                        log::debug!(
                            "Super tap: {} queued taps cancel out or are stale",
                            queued.len()
                        );
                        continue;
                    };
                    let outcome = toggler.tap(Instant::now());
                    let waited = tapped.elapsed();
                    match outcome {
                        TapOutcome::Failed(error) => {
                            log::warn!("Super tap: cannot reach or start the launcher: {error}");
                        }
                        outcome => log::debug!("Super tap: {outcome:?} ({waited:?} after the tap)"),
                    }
                }
            })?;
        Ok(LauncherClient { requests })
    }

    /// Queues a toggle; never blocks (safe from the event tap). Returns
    /// false when the client thread is gone.
    pub fn toggle(&self) -> bool {
        self.requests.send(Instant::now()).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::os::unix::net::UnixListener;

    /// A short temp dir under /tmp: socket paths are limited to 104 bytes.
    fn short_tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("cmwl")
            .tempdir_in("/tmp")
            .unwrap()
    }

    #[test]
    fn queued_toggles_are_dropped_when_stale_and_merged_by_parity() {
        let now = Instant::now();
        let max_age = Duration::from_secs(1);
        let ago = |ms: u64| now - Duration::from_millis(ms);
        // One fresh toggle goes through.
        assert_eq!(coalesce_toggles(&[ago(10)], now, max_age), Some(ago(10)));
        // Two fresh toggles cancel out, three make one.
        assert_eq!(coalesce_toggles(&[ago(300), ago(10)], now, max_age), None);
        assert_eq!(
            coalesce_toggles(&[ago(500), ago(300), ago(10)], now, max_age),
            Some(ago(10))
        );
        // Stale ones are ignored before counting.
        assert_eq!(coalesce_toggles(&[ago(5000)], now, max_age), None);
        assert_eq!(
            coalesce_toggles(&[ago(5000), ago(4000), ago(10)], now, max_age),
            Some(ago(10))
        );
        assert_eq!(coalesce_toggles(&[], now, max_age), None);
    }

    #[test]
    fn send_writes_one_line_and_reads_the_reply() {
        let dir = short_tempdir();
        let socket = dir.path().join("l.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for reply in ["ok\n", "error: busy\n"] {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                lines.push(line);
                (&stream).write_all(reply.as_bytes()).unwrap();
            }
            lines
        });
        assert_eq!(send(&socket, LauncherCommand::Toggle, IO_TIMEOUT), Ok(()));
        assert_eq!(
            send(&socket, LauncherCommand::Show, IO_TIMEOUT),
            Err("launcher: busy".to_owned())
        );
        assert_eq!(server.join().unwrap(), ["toggle\n", "show\n"]);
    }

    #[test]
    fn send_fails_without_a_listener() {
        let dir = short_tempdir();
        let error = send(
            &dir.path().join("none.sock"),
            LauncherCommand::Toggle,
            IO_TIMEOUT,
        )
        .unwrap_err();
        assert!(error.contains("cannot connect"), "{error}");
    }

    #[test]
    fn a_launcher_that_never_replies_still_counts_as_reached() {
        let dir = short_tempdir();
        let socket = dir.path().join("l.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            // Hold the connection open past the client's timeout.
            std::thread::sleep(Duration::from_millis(150));
            drop(stream);
            line
        });
        assert_eq!(
            send(&socket, LauncherCommand::Hide, Duration::from_millis(50)),
            Ok(())
        );
        assert_eq!(server.join().unwrap(), "hide\n");
    }

    #[test]
    fn spawn_plans_prefer_the_bundle_through_open() {
        let bin = Path::new("/Users/x/cosmicMac/bin");
        let log = Path::new("/Users/x/Library/Logs/cosmicMac/launcher.log");
        let all = |_: &Path| true;
        let plans = spawn_plans(bin, Some(log), all);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].program, Path::new("/usr/bin/open"));
        assert!(plans[0].via_open);
        assert_eq!(
            plans[0].args,
            [
                "-a",
                "/Users/x/cosmicMac/bin/cosmic-mac-launcher.app",
                "--stdout",
                "/Users/x/Library/Logs/cosmicMac/launcher.log",
                "--stderr",
                "/Users/x/Library/Logs/cosmicMac/launcher.log",
                "--args",
                "--show",
            ]
            .map(OsString::from)
        );
        assert_eq!(
            plans[1],
            SpawnPlan {
                program: bin.join("cosmic-mac-launcher"),
                args: vec!["--show".into()],
                via_open: false,
            }
        );

        let without_log = spawn_plans(bin, None, all);
        assert_eq!(
            without_log[0].args,
            [
                "-a",
                "/Users/x/cosmicMac/bin/cosmic-mac-launcher.app",
                "--args",
                "--show"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn spawn_plans_only_list_existing_targets() {
        let bin = Path::new("/b");
        let only_binary: HashSet<PathBuf> = [PathBuf::from("/b/cosmic-mac-launcher")].into();
        let plans = spawn_plans(bin, None, |path| only_binary.contains(path));
        assert_eq!(plans.len(), 1);
        assert!(!plans[0].via_open);
        assert!(spawn_plans(bin, None, |_| false).is_empty());
    }

    #[test]
    fn run_plan_reports_a_failing_open_and_starts_a_binary() {
        let failing = SpawnPlan {
            program: PathBuf::from("/usr/bin/false"),
            args: Vec::new(),
            via_open: true,
        };
        assert!(run_plan(&failing).unwrap_err().contains("failed"));
        let started = SpawnPlan {
            program: PathBuf::from("/usr/bin/true"),
            args: vec!["--show".into()],
            via_open: false,
        };
        assert_eq!(run_plan(&started), Ok(()));
        let missing = SpawnPlan {
            program: PathBuf::from("/nonexistent/cosmic-mac-launcher"),
            args: Vec::new(),
            via_open: false,
        };
        assert!(run_plan(&missing).unwrap_err().contains("cannot start"));
    }

    /// Scripted port: `reachable` answers sends, spawns are counted.
    #[derive(Default)]
    struct FakePort {
        reachable: bool,
        spawn_fails: bool,
        sends: usize,
        spawns: usize,
    }

    impl LauncherPort for FakePort {
        fn send(&mut self, command: LauncherCommand) -> Result<(), String> {
            assert_eq!(command, LauncherCommand::Toggle);
            self.sends += 1;
            if self.reachable {
                Ok(())
            } else {
                Err("no socket".into())
            }
        }

        fn spawn(&mut self) -> Result<(), String> {
            self.spawns += 1;
            if self.spawn_fails {
                Err("no launcher".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn a_reachable_launcher_gets_toggle_and_is_never_spawned() {
        let mut toggler = Toggler::new(
            FakePort {
                reachable: true,
                ..FakePort::default()
            },
            SPAWN_GRACE,
        );
        let now = Instant::now();
        assert_eq!(toggler.tap(now), TapOutcome::Sent);
        assert_eq!(toggler.tap(now), TapOutcome::Sent);
        assert_eq!(toggler.port().sends, 2);
        assert_eq!(toggler.port().spawns, 0);
    }

    #[test]
    fn an_unreachable_launcher_is_spawned_once_per_grace_period() {
        let mut toggler = Toggler::new(FakePort::default(), Duration::from_secs(5));
        let start = Instant::now();
        assert_eq!(toggler.tap(start), TapOutcome::Spawned);
        assert_eq!(
            toggler.tap(start + Duration::from_secs(1)),
            TapOutcome::StillStarting
        );
        assert_eq!(toggler.port().spawns, 1);
        // Still unreachable after the grace period: spawn again.
        assert_eq!(
            toggler.tap(start + Duration::from_secs(6)),
            TapOutcome::Spawned
        );
        assert_eq!(toggler.port().spawns, 2);
        assert_eq!(toggler.port().sends, 3);
    }

    #[test]
    fn a_failed_spawn_is_retried_on_the_next_tap() {
        let mut toggler = Toggler::new(
            FakePort {
                spawn_fails: true,
                ..FakePort::default()
            },
            SPAWN_GRACE,
        );
        let now = Instant::now();
        let TapOutcome::Failed(error) = toggler.tap(now) else {
            panic!("expected a failure");
        };
        assert!(
            error.contains("no socket") && error.contains("no launcher"),
            "{error}"
        );
        assert!(matches!(toggler.tap(now), TapOutcome::Failed(_)));
        assert_eq!(toggler.port().spawns, 2);
    }

    #[test]
    fn the_client_thread_sends_toggle_to_the_socket() {
        let dir = short_tempdir();
        let socket = dir.path().join("l.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let client = LauncherClient::spawn(SocketPort {
            socket: socket.clone(),
            bin_dir: dir.path().to_path_buf(),
            log: None,
        })
        .unwrap();
        assert!(client.toggle());
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        (&stream).write_all(b"ok\n").unwrap();
        assert_eq!(line, "toggle\n");
    }
}
