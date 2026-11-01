// SPDX-License-Identifier: GPL-3.0-or-later
//! The launcher's control socket: a line protocol between its clients (the
//! window manager's Super tap, `cosmic-mac-launcher --show`) and the resident
//! launcher process.
//!
//! * Socket: `~/Library/Application Support/cosmic/cosmic-mac-launcher.sock`,
//!   next to the window manager's socket, mode 0600.
//! * Request: one [`LauncherCommand`] per line (`toggle`, `show`, `hide`;
//!   ASCII case and surrounding whitespace are ignored, empty lines skipped).
//!   A connection may carry several lines.
//! * Reply: one line per request, [`REPLY_OK`] or `error: <reason>`. Clients
//!   may close the connection without reading it.
//!
//! The protocol is plain text rather than the WM's JSON envelope so a shell
//! can drive it: `echo toggle | nc -U ~/Library/Application\ Support/cosmic/cosmic-mac-launcher.sock`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// File name of the socket inside `~/Library/Application Support/cosmic/`.
pub const SOCKET_FILE_NAME: &str = "cosmic-mac-launcher.sock";

/// Socket path relative to the home directory.
pub const SOCKET_RELATIVE_PATH: &str =
    "Library/Application Support/cosmic/cosmic-mac-launcher.sock";

/// Longest request line the launcher reads; longer lines are an error.
pub const MAX_LINE_BYTES: usize = 256;

/// Reply to a request that was accepted.
pub const REPLY_OK: &str = "ok";

/// The launcher socket for a home directory.
pub fn socket_path(home: &Path) -> PathBuf {
    home.join(SOCKET_RELATIVE_PATH)
}

/// A request to the resident launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LauncherCommand {
    /// Hide when shown (or opening), show otherwise.
    Toggle,
    /// Show with an empty query, centred on the main display, with
    /// keyboard focus; re-focuses an already visible launcher.
    Show,
    /// Hide; nothing happens when already hidden.
    Hide,
}

impl LauncherCommand {
    /// Every command, for help texts and tests.
    pub const ALL: [LauncherCommand; 3] = [
        LauncherCommand::Toggle,
        LauncherCommand::Show,
        LauncherCommand::Hide,
    ];

    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            LauncherCommand::Toggle => "toggle",
            LauncherCommand::Show => "show",
            LauncherCommand::Hide => "hide",
        }
    }

    /// The request line, newline included.
    pub fn line(self) -> String {
        format!("{}\n", self.as_str())
    }
}

impl fmt::Display for LauncherCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LauncherCommand {
    type Err = String;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let word = line.trim();
        LauncherCommand::ALL
            .into_iter()
            .find(|command| command.as_str().eq_ignore_ascii_case(word))
            .ok_or_else(|| format!("unknown command {word:?} (expected toggle, show or hide)"))
    }
}

/// The error reply for `reason` (single line).
pub fn error_reply(reason: &str) -> String {
    let reason: String = reason
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    format!("error: {reason}")
}

/// Interprets a reply line: `Ok` for [`REPLY_OK`], the reason for an error.
pub fn parse_reply(line: &str) -> Result<(), String> {
    let line = line.trim();
    if line == REPLY_OK {
        return Ok(());
    }
    Err(line
        .strip_prefix("error:")
        .map(str::trim)
        .unwrap_or(line)
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_and_ignore_case_and_whitespace() {
        for command in LauncherCommand::ALL {
            assert_eq!(command.as_str().parse(), Ok(command));
            assert_eq!(command.line().parse(), Ok(command));
            assert_eq!(command.to_string(), command.as_str());
        }
        assert_eq!("  TOGGLE \r\n".parse(), Ok(LauncherCommand::Toggle));
        assert_eq!("Show".parse(), Ok(LauncherCommand::Show));
    }

    #[test]
    fn unknown_commands_are_errors() {
        for line in ["", "  ", "quit", "show now", "toggle;hide", "hidden"] {
            let error = line.parse::<LauncherCommand>().unwrap_err();
            assert!(error.contains("unknown command"), "{line:?}: {error}");
        }
    }

    #[test]
    fn replies_round_trip() {
        assert_eq!(parse_reply("ok\n"), Ok(()));
        assert_eq!(
            parse_reply(&error_reply("unknown command \"x\"")),
            Err("unknown command \"x\"".to_owned())
        );
        assert_eq!(error_reply("two\nlines"), "error: two lines");
        assert_eq!(parse_reply("garbage"), Err("garbage".to_owned()));
    }

    #[test]
    fn socket_lives_next_to_the_window_manager_socket() {
        assert_eq!(
            socket_path(Path::new("/Users/someone")),
            Path::new("/Users/someone/Library/Application Support/cosmic/cosmic-mac-launcher.sock")
        );
        assert!(SOCKET_RELATIVE_PATH.ends_with(SOCKET_FILE_NAME));
        // Unix socket paths are limited to 104 bytes on macOS; a typical home
        // leaves plenty of room.
        assert!(socket_path(Path::new("/Users/someone")).as_os_str().len() < 104);
    }
}
