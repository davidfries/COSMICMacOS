// SPDX-License-Identifier: GPL-3.0-or-later
//! The resident launcher's control socket (server side, protocol in
//! `cosmic_mac_protocol::launcher`) and its single-instance guarantee.
//!
//! Single instance: the launcher takes an exclusive `flock` on
//! `cosmic-mac-launcher.lock` next to the socket before it binds. Whoever
//! holds the lock owns the socket path, so a socket file found then is stale
//! and is replaced. A second launcher finds the lock taken; started with
//! `--show` it forwards `show` to the first (retrying while that one is
//! still starting) and exits. The lock dies with the process, so a crash
//! never blocks the next start, and two launchers racing at login cannot
//! both end up listening.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmic_mac_protocol::launcher::{self as protocol, LauncherCommand};

/// Read and write timeout of one connection.
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(1);

/// How long `--show` keeps trying to reach a launcher that holds the lock.
pub const FORWARD_PATIENCE: Duration = Duration::from_secs(3);

/// Receives every accepted command; returns false once the app is gone.
pub type Sink = Arc<dyn Fn(LauncherCommand) -> bool + Send + Sync>;

/// The lock file guarding `socket`.
pub fn lock_path(socket: &Path) -> PathBuf {
    socket.with_extension("lock")
}

/// The outcome of [`claim`].
pub enum Claim {
    /// This process is the launcher: it holds the lock and listens.
    Primary(Instance),
    /// Another launcher holds the lock.
    Secondary,
}

/// The lock and the bound socket of the primary launcher.
pub struct Instance {
    lock: File,
    listener: UnixListener,
    socket: PathBuf,
}

/// Takes the single-instance lock and binds `socket` (mode 0600).
pub fn claim(socket: &Path) -> io::Result<Claim> {
    if let Some(dir) = socket.parent() {
        fs::create_dir_all(dir)?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path(socket))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(Claim::Secondary),
        Err(TryLockError::Error(error)) => return Err(error),
    }
    match fs::remove_file(socket) {
        Ok(()) => log::info!("replaced the stale socket {}", socket.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok(Claim::Primary(Instance {
        lock,
        listener,
        socket: socket.to_path_buf(),
    }))
}

impl Instance {
    /// Serves the socket on the `launcher-ipc` thread (one short-lived
    /// thread per connection). Keep the returned guard for as long as the
    /// launcher runs: it holds the lock and removes the socket when dropped.
    pub fn serve(self, sink: Sink) -> io::Result<ServerGuard> {
        let Instance {
            lock,
            listener,
            socket,
        } = self;
        std::thread::Builder::new()
            .name("launcher-ipc".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let stream = match stream {
                        Ok(stream) => stream,
                        Err(error) => {
                            log::warn!("launcher socket accept failed: {error}");
                            continue;
                        }
                    };
                    let sink = sink.clone();
                    let spawned = std::thread::Builder::new()
                        .name("launcher-ipc-conn".into())
                        .spawn(move || {
                            if let Err(error) = handle_connection(stream, &*sink) {
                                log::debug!("launcher socket connection: {error}");
                            }
                        });
                    if let Err(error) = spawned {
                        log::warn!("cannot spawn a launcher socket thread: {error}");
                    }
                }
            })?;
        Ok(ServerGuard {
            _lock: lock,
            socket,
        })
    }
}

/// Holds the single-instance lock; removes the socket file on drop.
pub struct ServerGuard {
    _lock: File,
    socket: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        // Still holding the lock, so the file is ours.
        let _ = fs::remove_file(&self.socket);
    }
}

/// Reads request lines until EOF, answering each.
fn handle_connection(stream: UnixStream, sink: &dyn Fn(LauncherCommand) -> bool) -> io::Result<()> {
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    loop {
        let mut line = Vec::new();
        let limit = protocol::MAX_LINE_BYTES as u64 + 1;
        if (&mut reader).take(limit).read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.len() > protocol::MAX_LINE_BYTES && !line.ends_with(b"\n") {
            // The rest of the line cannot be told from the next request.
            let reply = protocol::error_reply(&format!(
                "request longer than {} bytes",
                protocol::MAX_LINE_BYTES
            ));
            writer.write_all(format!("{reply}\n").as_bytes())?;
            return Ok(());
        }
        let text = String::from_utf8_lossy(&line);
        if text.trim().is_empty() {
            continue;
        }
        let reply = respond(&text, sink);
        writer.write_all(format!("{reply}\n").as_bytes())?;
    }
}

/// The reply to one request line, after handing a valid command to `sink`.
pub fn respond(line: &str, sink: &dyn Fn(LauncherCommand) -> bool) -> String {
    match line.parse::<LauncherCommand>() {
        Ok(command) => {
            log::debug!("socket: {command}");
            if sink(command) {
                protocol::REPLY_OK.to_owned()
            } else {
                protocol::error_reply("the launcher is shutting down")
            }
        }
        Err(error) => protocol::error_reply(&error),
    }
}

/// Sends `command` to the launcher at `socket` and reads its reply.
pub fn request(socket: &Path, command: LauncherCommand, timeout: Duration) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(command.line().as_bytes())
        .map_err(|error| format!("cannot send {command}: {error}"))?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|error| format!("no reply to {command}: {error}"))?;
    protocol::parse_reply(&reply).map_err(|reason| format!("launcher: {reason}"))
}

/// [`request`], retrying failed connections for up to `patience` (the
/// running launcher may hold the lock without listening yet).
pub fn forward(socket: &Path, command: LauncherCommand, patience: Duration) -> Result<(), String> {
    let deadline = Instant::now() + patience;
    loop {
        match UnixStream::connect(socket) {
            Ok(_) => return request(socket, command, CONNECTION_TIMEOUT),
            Err(error) if Instant::now() >= deadline => {
                return Err(format!(
                    "another launcher holds {} but {} does not answer: {error}",
                    lock_path(socket).display(),
                    socket.display()
                ));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::sync::Mutex;
    use std::sync::mpsc;

    fn primary(socket: &Path) -> Instance {
        match claim(socket).unwrap() {
            Claim::Primary(instance) => instance,
            Claim::Secondary => panic!("expected to own the lock"),
        }
    }

    fn channel_sink() -> (Sink, mpsc::Receiver<LauncherCommand>) {
        let (sender, receiver) = mpsc::channel();
        let sender = Mutex::new(sender);
        let sink: Sink = Arc::new(move |command| sender.lock().unwrap().send(command).is_ok());
        (sink, receiver)
    }

    #[test]
    fn respond_accepts_commands_and_reports_errors() {
        let accepted = Mutex::new(Vec::new());
        let sink = |command| {
            accepted.lock().unwrap().push(command);
            true
        };
        assert_eq!(respond("toggle\n", &sink), "ok");
        assert_eq!(respond(" Show ", &sink), "ok");
        assert!(respond("quit", &sink).starts_with("error: unknown command"));
        assert_eq!(
            *accepted.lock().unwrap(),
            [LauncherCommand::Toggle, LauncherCommand::Show]
        );
        let gone = |_| false;
        assert_eq!(
            respond("hide", &gone),
            "error: the launcher is shutting down"
        );
    }

    #[test]
    fn second_claim_is_secondary_until_the_first_is_dropped() {
        let tmp = TempDir::new("ipc-claim");
        let socket = tmp.path().join("l.sock");
        let first = primary(&socket);
        assert!(matches!(claim(&socket).unwrap(), Claim::Secondary));
        let mode = fs::metadata(&socket).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(first);
        assert!(matches!(claim(&socket).unwrap(), Claim::Primary(_)));
    }

    #[test]
    fn a_stale_socket_file_is_replaced() {
        let tmp = TempDir::new("ipc-stale");
        let socket = tmp.path().join("l.sock");
        fs::write(&socket, "left over").unwrap();
        let instance = primary(&socket);
        let (sink, receiver) = channel_sink();
        let _guard = instance.serve(sink).unwrap();
        assert_eq!(
            request(&socket, LauncherCommand::Show, CONNECTION_TIMEOUT),
            Ok(())
        );
        assert_eq!(receiver.recv().unwrap(), LauncherCommand::Show);
    }

    #[test]
    fn serves_several_lines_per_connection_and_rejects_bad_ones() {
        let tmp = TempDir::new("ipc-lines");
        let socket = tmp.path().join("l.sock");
        let (sink, receiver) = channel_sink();
        let _guard = primary(&socket).serve(sink).unwrap();

        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.write_all(b"toggle\n\nhide\nbogus\nSHOW\n").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut replies = String::new();
        BufReader::new(stream).read_to_string(&mut replies).unwrap();
        let replies: Vec<&str> = replies.lines().collect();
        assert_eq!(replies.len(), 4, "{replies:?}");
        assert_eq!(replies[0], "ok");
        assert_eq!(replies[1], "ok");
        assert!(
            replies[2].starts_with("error: unknown command"),
            "{replies:?}"
        );
        assert_eq!(replies[3], "ok");
        let received: Vec<LauncherCommand> = receiver.try_iter().collect();
        assert_eq!(
            received,
            [
                LauncherCommand::Toggle,
                LauncherCommand::Hide,
                LauncherCommand::Show
            ]
        );
    }

    #[test]
    fn overlong_lines_are_refused() {
        let tmp = TempDir::new("ipc-long");
        let socket = tmp.path().join("l.sock");
        let (sink, receiver) = channel_sink();
        let _guard = primary(&socket).serve(sink).unwrap();
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .write_all(&vec![b'x'; protocol::MAX_LINE_BYTES + 10])
            .unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        assert!(reply.contains("longer than"), "{reply}");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn forward_reaches_a_listening_launcher_and_gives_up_otherwise() {
        let tmp = TempDir::new("ipc-fwd");
        let socket = tmp.path().join("l.sock");
        let error = forward(&socket, LauncherCommand::Show, Duration::ZERO).unwrap_err();
        assert!(error.contains("does not answer"), "{error}");

        // The launcher starts listening while `forward` is already waiting.
        let (sink, receiver) = channel_sink();
        let delayed_socket = socket.clone();
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            primary(&delayed_socket).serve(sink).unwrap()
        });
        assert_eq!(
            forward(&socket, LauncherCommand::Show, Duration::from_secs(3)),
            Ok(())
        );
        assert_eq!(receiver.recv().unwrap(), LauncherCommand::Show);
        let guard = server.join().unwrap();
        drop(guard);
        assert!(!socket.exists(), "the guard removes the socket");
    }
}
