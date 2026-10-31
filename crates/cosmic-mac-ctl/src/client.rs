// SPDX-License-Identifier: GPL-3.0-or-later
//! Unix-socket connection handling.
//!
//! The daemon listens on `~/Library/Application Support/cosmic/cosmic-mac-wm.sock` and speaks
//! one JSON document per line in each direction.  The path is resolved from `HOME` (not
//! `TMPDIR`) because non-interactive SSH sessions may not set `TMPDIR`.  `--socket` and the
//! `COSMIC_MAC_WM_SOCKET` environment variable override it; tests rely on both.

use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cosmic_mac_protocol::{Request, Response};
use serde_json::Value;

/// Default reply timeout, in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 2_000;

/// Refuse absurdly large replies instead of allocating without bound.
pub const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;

/// Relative path of the socket inside the user's home directory.
pub const SOCKET_RELATIVE_PATH: &str = "Library/Application Support/cosmic/cosmic-mac-wm.sock";

/// Everything that can go wrong while talking to the daemon.
#[derive(Debug)]
pub enum ClientError {
    /// `HOME` is unset, so the default socket path cannot be built.
    HomeUnavailable,
    /// `UnixStream::connect` failed (missing socket, refused, permissions, …).
    Connect { path: PathBuf, source: io::Error },
    /// Writing the request line failed.
    Write { path: PathBuf, source: io::Error },
    /// Reading the reply line failed.
    Read { path: PathBuf, source: io::Error },
    /// No reply arrived within the configured timeout.
    Timeout { path: PathBuf, timeout: Duration },
    /// The daemon closed the connection without sending a line.
    Closed { path: PathBuf },
    /// The reply exceeded [`MAX_REPLY_BYTES`].
    OversizedReply { path: PathBuf, limit: usize },
    /// The reply was not valid JSON for [`Response`].
    InvalidReply {
        path: PathBuf,
        line: String,
        source: serde_json::Error,
    },
    /// The request could not be encoded (a programming error, surfaced instead of panicking).
    Encode(serde_json::Error),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::HomeUnavailable => write!(
                f,
                "cannot determine the socket path: HOME is not set\n  \
                 hint: pass --socket <path> or set COSMIC_MAC_WM_SOCKET"
            ),
            ClientError::Connect { path, source } => match source.kind() {
                io::ErrorKind::NotFound => write!(
                    f,
                    "daemon not running: no socket at {}\n  \
                     hint: start it with `cosmic-mac-wm`, or pass --socket <path>",
                    path.display()
                ),
                io::ErrorKind::ConnectionRefused => write!(
                    f,
                    "daemon not running: {} exists but nothing is listening\n  \
                     hint: start it with `cosmic-mac-wm`, or pass --socket <path>",
                    path.display()
                ),
                io::ErrorKind::PermissionDenied => write!(
                    f,
                    "cannot connect to {}: permission denied\n  \
                     hint: check the socket and directory permissions",
                    path.display()
                ),
                _ => write!(
                    f,
                    "cannot connect to {}: {}\n  \
                     hint: is the daemon running? start it with `cosmic-mac-wm`",
                    path.display(),
                    source
                ),
            },
            ClientError::Write { path, source } => {
                write!(
                    f,
                    "failed to send request to {}: {}",
                    path.display(),
                    source
                )
            }
            ClientError::Read { path, source } => {
                write!(
                    f,
                    "failed to read reply from {}: {}",
                    path.display(),
                    source
                )
            }
            ClientError::Timeout { path, timeout } => write!(
                f,
                "timed out after {} ms waiting for a reply from {}\n  \
                 hint: the daemon may be hung; raise the limit with --timeout-ms",
                timeout.as_millis(),
                path.display()
            ),
            ClientError::Closed { path } => write!(
                f,
                "daemon at {} closed the connection without replying",
                path.display()
            ),
            ClientError::OversizedReply { path, limit } => write!(
                f,
                "reply from {} exceeds the {} byte limit",
                path.display(),
                limit
            ),
            ClientError::InvalidReply { path, line, source } => write!(
                f,
                "invalid reply from {}: {}\n  reply: {}",
                path.display(),
                source,
                truncate_for_error(line)
            ),
            ClientError::Encode(source) => write!(f, "failed to encode request: {source}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Connect { source, .. }
            | ClientError::Write { source, .. }
            | ClientError::Read { source, .. } => Some(source),
            ClientError::InvalidReply { source, .. } => Some(source),
            ClientError::Encode(source) => Some(source),
            _ => None,
        }
    }
}

fn truncate_for_error(line: &str) -> String {
    let line = line.trim_end();
    let mut chars = line.chars();
    let short: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

/// Resolve the socket path using the process environment.
pub fn resolve_socket_path(explicit: Option<&Path>) -> Result<PathBuf, ClientError> {
    let env_socket = env::var_os("COSMIC_MAC_WM_SOCKET");
    let home = env::var_os("HOME");
    resolve_socket_path_from(explicit, env_socket.as_deref(), home.as_deref())
}

/// Pure socket-path resolution, split out so precedence is unit-testable.
///
/// Precedence: explicit `--socket`, then `COSMIC_MAC_WM_SOCKET`, then
/// `$HOME/Library/Application Support/cosmic/cosmic-mac-wm.sock`.
pub fn resolve_socket_path_from(
    explicit: Option<&Path>,
    env_socket: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, ClientError> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(value) = env_socket.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    let home = home
        .filter(|value| !value.is_empty())
        .ok_or(ClientError::HomeUnavailable)?;
    Ok(PathBuf::from(home).join(SOCKET_RELATIVE_PATH))
}

/// Resolve the timeout from `--timeout-ms` or `COSMIC_MAC_WM_TIMEOUT_MS`.
pub fn resolve_timeout(explicit_ms: Option<u64>) -> Result<Duration, String> {
    let env_value = env::var("COSMIC_MAC_WM_TIMEOUT_MS").ok();
    let ms = parse_timeout_ms(explicit_ms, env_value.as_deref())?;
    Ok(Duration::from_millis(ms))
}

/// Pure timeout parsing (see [`resolve_timeout`]).
pub fn parse_timeout_ms(explicit_ms: Option<u64>, env_value: Option<&str>) -> Result<u64, String> {
    let ms = match explicit_ms {
        Some(ms) => ms,
        None => match env_value {
            Some(value) => value.trim().parse::<u64>().map_err(|_| {
                format!("invalid COSMIC_MAC_WM_TIMEOUT_MS value `{value}` (expected milliseconds)")
            })?,
            None => DEFAULT_TIMEOUT_MS,
        },
    };
    if ms == 0 {
        return Err("timeout must be at least 1 ms".to_string());
    }
    if ms > 3_600_000 {
        return Err("timeout must not exceed 3600000 ms (one hour)".to_string());
    }
    Ok(ms)
}

/// One decoded reply.
///
/// `data` is the raw `data` field from the reply line.  Keeping it lets `--json` print exactly
/// what the daemon sent (including fields this CLI version does not know about) instead of a
/// re-encoding of the typed [`Response`].
#[derive(Debug, Clone)]
pub struct Reply {
    pub response: Response,
    pub data: Option<Value>,
}

/// A client bound to one socket path and timeout.
///
/// One request is sent per connection: the daemon contract is a single JSON line in each
/// direction, and closing the stream after the reply keeps state out of the CLI.
#[derive(Debug, Clone)]
pub struct Client {
    path: PathBuf,
    timeout: Duration,
}

impl Client {
    pub fn new(path: PathBuf, timeout: Duration) -> Self {
        Client { path, timeout }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Send one request and read one reply.
    pub fn request(&self, request: &Request) -> Result<Reply, ClientError> {
        let payload = serde_json::to_string(request).map_err(ClientError::Encode)?;

        let mut stream =
            UnixStream::connect(&self.path).map_err(|source| ClientError::Connect {
                path: self.path.clone(),
                source,
            })?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|source| ClientError::Read {
                path: self.path.clone(),
                source,
            })?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|source| ClientError::Write {
                path: self.path.clone(),
                source,
            })?;

        stream
            .write_all(payload.as_bytes())
            .and_then(|()| stream.write_all(b"\n"))
            .and_then(|()| stream.flush())
            .map_err(|source| ClientError::Write {
                path: self.path.clone(),
                source,
            })?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let read = reader.read_line(&mut line).map_err(|source| {
            if matches!(
                source.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) {
                ClientError::Timeout {
                    path: self.path.clone(),
                    timeout: self.timeout,
                }
            } else {
                ClientError::Read {
                    path: self.path.clone(),
                    source,
                }
            }
        })?;

        if read == 0 {
            return Err(ClientError::Closed {
                path: self.path.clone(),
            });
        }
        if line.len() > MAX_REPLY_BYTES {
            return Err(ClientError::OversizedReply {
                path: self.path.clone(),
                limit: MAX_REPLY_BYTES,
            });
        }

        let value: Value =
            serde_json::from_str(line.trim_end()).map_err(|source| ClientError::InvalidReply {
                path: self.path.clone(),
                line: line.trim_end().to_string(),
                source,
            })?;
        let response: Response =
            serde_json::from_value(value.clone()).map_err(|source| ClientError::InvalidReply {
                path: self.path.clone(),
                line: line.trim_end().to_string(),
                source,
            })?;
        let data = value.get("data").cloned();

        Ok(Reply { response, data })
    }
}

/// Build a default path for tests and docs without touching the environment.
pub fn default_socket_path_for_home(home: impl AsRef<OsStr>) -> PathBuf {
    PathBuf::from(home.as_ref()).join(SOCKET_RELATIVE_PATH)
}

#[cfg(test)]
mod tests {
    use super::{
        ClientError, DEFAULT_TIMEOUT_MS, default_socket_path_for_home, parse_timeout_ms,
        resolve_socket_path_from,
    };
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    #[test]
    fn explicit_socket_wins_over_env_and_home() {
        let path = resolve_socket_path_from(
            Some(Path::new("/tmp/explicit.sock")),
            Some(OsStr::new("/tmp/env.sock")),
            Some(OsStr::new("/Users/tester")),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/tmp/explicit.sock"));
    }

    #[test]
    fn env_socket_wins_over_home() {
        let path = resolve_socket_path_from(
            None,
            Some(OsStr::new("/tmp/env.sock")),
            Some(OsStr::new("/Users/tester")),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/tmp/env.sock"));
    }

    #[test]
    fn empty_env_socket_is_ignored() {
        let path = resolve_socket_path_from(
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("/Users/tester")),
        )
        .unwrap();
        assert_eq!(path, default_socket_path_for_home("/Users/tester"));
        assert!(path.ends_with("Library/Application Support/cosmic/cosmic-mac-wm.sock"));
    }

    #[test]
    fn missing_home_is_an_error() {
        let error = resolve_socket_path_from(None, None, None).unwrap_err();
        assert!(matches!(error, ClientError::HomeUnavailable));
        assert!(error.to_string().contains("HOME is not set"));
    }

    #[test]
    fn timeout_defaults_and_overrides() {
        assert_eq!(parse_timeout_ms(None, None).unwrap(), DEFAULT_TIMEOUT_MS);
        assert_eq!(parse_timeout_ms(Some(50), None).unwrap(), 50);
        assert_eq!(parse_timeout_ms(None, Some("750")).unwrap(), 750);
        assert_eq!(parse_timeout_ms(Some(50), Some("750")).unwrap(), 50);
    }

    #[test]
    fn invalid_timeouts_are_rejected() {
        assert!(parse_timeout_ms(Some(0), None).is_err());
        assert!(parse_timeout_ms(None, Some("nope")).is_err());
        assert!(parse_timeout_ms(None, Some("99999999")).is_err());
    }
}
