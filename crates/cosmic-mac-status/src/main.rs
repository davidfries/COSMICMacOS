// SPDX-License-Identifier: GPL-3.0-or-later
//! `cosmic-mac-status`: the cosmicMac menu-bar status item.
//!
//! The menu bar shows the active workspace; the menu switches workspaces,
//! toggles tiling for the active workspace, floats the focused window and
//! quits the status item.  It is a separate process from the window manager,
//! which has no `NSApplication`: it subscribes to the WM's IPC socket
//! (`Request::Subscribe`) and sends menu actions as one-shot requests.  While
//! the WM is not reachable it shows a neutral glyph and reconnects with
//! backoff.
//!
//! * [`model`] — the pure state → title/menu mapping (unit tested).
//! * [`connection`] — the subscription and action threads.
//! * [`appkit`] — the thin `NSStatusItem` / `NSMenu` glue on the main thread.

mod appkit;
mod connection;
mod model;

use std::process::ExitCode;
use std::time::Duration;

use cosmic_mac_ctl::client::{self, Client, DEFAULT_TIMEOUT_MS};

const USAGE: &str = "\
cosmic-mac-status — cosmicMac menu-bar status item

Usage:
  cosmic-mac-status [-h | --help | -V | --version]

Shows the active workspace in the menu bar, with a menu to switch workspaces,
toggle tiling and float the focused window.  Talks to cosmic-mac-wm over
~/Library/Application Support/cosmic/cosmic-mac-wm.sock ($COSMIC_MAC_WM_SOCKET
overrides it) and waits for the window manager when it is not running.
Normally started by launchd (`cosmic-mac-session install`).  Logs go to stderr;
RUST_LOG sets the level (default info).
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Run,
    Help,
    Version,
}

fn parse_args(args: &[String]) -> Result<Mode, String> {
    match args {
        [] => Ok(Mode::Run),
        [flag] if flag == "-h" || flag == "--help" => Ok(Mode::Help),
        [flag] if flag == "-V" || flag == "--version" => Ok(Mode::Version),
        _ => Err(format!("unexpected arguments: {}", args.join(" "))),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(Mode::Run) => {}
        Ok(Mode::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Mode::Version) => {
            println!("cosmic-mac-status {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("cosmic-mac-status: {error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let path = match client::resolve_socket_path(None) {
        Ok(path) => path,
        Err(error) => {
            log::error!("{error}");
            return ExitCode::FAILURE;
        }
    };
    log::info!(
        "cosmic-mac-status {} (window manager socket {})",
        env!("CARGO_PKG_VERSION"),
        path.display()
    );
    appkit::run(Client::new(path, Duration::from_millis(DEFAULT_TIMEOUT_MS)))
}

#[cfg(test)]
mod tests {
    use super::{Mode, parse_args};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn arguments() {
        assert_eq!(parse_args(&args(&[])), Ok(Mode::Run));
        assert_eq!(parse_args(&args(&["--help"])), Ok(Mode::Help));
        assert_eq!(parse_args(&args(&["-h"])), Ok(Mode::Help));
        assert_eq!(parse_args(&args(&["-V"])), Ok(Mode::Version));
        assert_eq!(parse_args(&args(&["--version"])), Ok(Mode::Version));
        assert!(parse_args(&args(&["--socket"])).is_err());
        assert!(parse_args(&args(&["--help", "extra"])).is_err());
    }
}
