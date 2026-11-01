// SPDX-License-Identifier: GPL-3.0-or-later
//! Hand-rolled command-line parsing (like `cosmic-mac-ctl`; the surface is
//! three subcommands and two options, not worth a CLI framework).

use std::fmt;
use std::path::PathBuf;

pub const GLOBAL_HELP: &str = "\
cosmic-mac-session — install the cosmicMac launchd agents

Usage:
  cosmic-mac-session <COMMAND> [OPTIONS]

Commands:
  install [--bin-dir DIR] [--dry-run]   Write the agent plists and (re)load them
  uninstall [--dry-run]                 Stop the agents and remove their plists
  status                                Show which agents are installed and loaded

Options:
  -h, --help       Show help; use `<COMMAND> --help` for command details
  -V, --version    Show the version

Agents live in ~/Library/LaunchAgents (launchd domain gui/<uid>) and log to
~/Library/Logs/cosmicMac/; `status` lists them.

Exit codes:
  0  success
  1  the operation failed
  2  usage error
";

pub const INSTALL_HELP: &str = "\
cosmic-mac-session install — install and (re)load the launchd agents

Usage:
  cosmic-mac-session install [--bin-dir DIR] [--dry-run]

Options:
  --bin-dir DIR   Directory holding the agent programs (cosmic-mac-wm)
                  (default: the directory of this executable)
  --dry-run       Print every change (launchctl bootstrap/bootout, file writes,
                  directories) instead of performing it. Read-only queries
                  (`launchctl print`) still run, so the plan matches this machine.

Writes ~/Library/LaunchAgents/<label>.plist for each agent (RunAtLoad,
restarted by launchd after a crash but not after a clean exit) and loads it
with `launchctl bootstrap gui/<uid>`, window manager first. An agent that is
already loaded is booted out first, so running install again picks up new
binaries and plists.
";

pub const UNINSTALL_HELP: &str = "\
cosmic-mac-session uninstall — stop and remove the launchd agents

Usage:
  cosmic-mac-session uninstall [--dry-run]

Options:
  --dry-run       Print every change instead of performing it

Boots each agent out of gui/<uid> (launchd sends SIGTERM; cosmic-mac-wm
restores every window before it exits) and removes its plist. Agents that are
not loaded or not installed are skipped.
";

pub const STATUS_HELP: &str = "\
cosmic-mac-session status — show the launchd agents

Usage:
  cosmic-mac-session status

For each agent: whether its plist is installed, whether launchd has it loaded
(state, pid, last exit code, program) and where it logs.
";

pub const USAGE_SHORT: &str = "Usage: cosmic-mac-session <install|uninstall|status> [OPTIONS]\n\
     Try `cosmic-mac-session --help` for more information.";

/// A parsed subcommand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Install {
        bin_dir: Option<PathBuf>,
        dry_run: bool,
    },
    Uninstall {
        dry_run: bool,
    },
    Status,
}

/// Result of parsing the argument vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Command),
    Help(&'static str),
    Version,
}

/// A usage error (exit code 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// Parses the process arguments (without `argv[0]`).
pub fn parse<I>(args: I) -> Result<Parsed, UsageError>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let Some(first) = args.first() else {
        return Ok(Parsed::Help(GLOBAL_HELP));
    };
    let rest = &args[1..];
    match first.as_str() {
        "-h" | "--help" => Ok(Parsed::Help(GLOBAL_HELP)),
        "-V" | "--version" => Ok(Parsed::Version),
        "help" => Ok(Parsed::Help(help_for(rest.first().map(String::as_str))?)),
        "install" => parse_install(rest),
        "uninstall" => parse_uninstall(rest),
        "status" => parse_status(rest),
        other if other.starts_with('-') => Err(UsageError(format!("unknown option `{other}`"))),
        other => Err(UsageError(format!("unknown command `{other}`"))),
    }
}

fn help_for(topic: Option<&str>) -> Result<&'static str, UsageError> {
    match topic {
        None => Ok(GLOBAL_HELP),
        Some("install") => Ok(INSTALL_HELP),
        Some("uninstall") => Ok(UNINSTALL_HELP),
        Some("status") => Ok(STATUS_HELP),
        Some(other) => Err(UsageError(format!("no help for unknown command `{other}`"))),
    }
}

fn is_help(arg: &str) -> bool {
    arg == "-h" || arg == "--help"
}

fn parse_install(args: &[String]) -> Result<Parsed, UsageError> {
    let mut bin_dir = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            _ if is_help(arg) => return Ok(Parsed::Help(INSTALL_HELP)),
            "--dry-run" => dry_run = true,
            "--bin-dir" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| UsageError("--bin-dir requires a directory".into()))?;
                bin_dir = Some(non_empty_path(value)?);
            }
            _ if arg.starts_with("--bin-dir=") => {
                bin_dir = Some(non_empty_path(&arg["--bin-dir=".len()..])?);
            }
            other => return Err(unexpected("install", other)),
        }
        index += 1;
    }
    Ok(Parsed::Run(Command::Install { bin_dir, dry_run }))
}

fn parse_uninstall(args: &[String]) -> Result<Parsed, UsageError> {
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            arg if is_help(arg) => return Ok(Parsed::Help(UNINSTALL_HELP)),
            "--dry-run" => dry_run = true,
            other => return Err(unexpected("uninstall", other)),
        }
    }
    Ok(Parsed::Run(Command::Uninstall { dry_run }))
}

fn parse_status(args: &[String]) -> Result<Parsed, UsageError> {
    match args.first().map(String::as_str) {
        None => Ok(Parsed::Run(Command::Status)),
        Some(arg) if is_help(arg) => Ok(Parsed::Help(STATUS_HELP)),
        Some(other) => Err(unexpected("status", other)),
    }
}

fn non_empty_path(value: &str) -> Result<PathBuf, UsageError> {
    if value.is_empty() {
        Err(UsageError("--bin-dir requires a directory".into()))
    } else {
        Ok(PathBuf::from(value))
    }
}

fn unexpected(command: &str, arg: &str) -> UsageError {
    if arg.starts_with('-') {
        UsageError(format!("{command}: unknown option `{arg}`"))
    } else {
        UsageError(format!("{command}: unexpected argument `{arg}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Parsed, UsageError> {
        parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn no_arguments_and_help_flags_print_global_help() {
        assert_eq!(parse_strs(&[]), Ok(Parsed::Help(GLOBAL_HELP)));
        assert_eq!(parse_strs(&["--help"]), Ok(Parsed::Help(GLOBAL_HELP)));
        assert_eq!(parse_strs(&["-h"]), Ok(Parsed::Help(GLOBAL_HELP)));
        assert_eq!(parse_strs(&["help"]), Ok(Parsed::Help(GLOBAL_HELP)));
        assert_eq!(parse_strs(&["-V"]), Ok(Parsed::Version));
        assert_eq!(parse_strs(&["--version"]), Ok(Parsed::Version));
    }

    #[test]
    fn command_help() {
        assert_eq!(
            parse_strs(&["help", "install"]),
            Ok(Parsed::Help(INSTALL_HELP))
        );
        assert_eq!(
            parse_strs(&["install", "--dry-run", "--help"]),
            Ok(Parsed::Help(INSTALL_HELP))
        );
        assert_eq!(
            parse_strs(&["uninstall", "-h"]),
            Ok(Parsed::Help(UNINSTALL_HELP))
        );
        assert_eq!(
            parse_strs(&["status", "--help"]),
            Ok(Parsed::Help(STATUS_HELP))
        );
        assert!(parse_strs(&["help", "bogus"]).is_err());
    }

    #[test]
    fn install_options() {
        assert_eq!(
            parse_strs(&["install"]),
            Ok(Parsed::Run(Command::Install {
                bin_dir: None,
                dry_run: false
            }))
        );
        assert_eq!(
            parse_strs(&["install", "--bin-dir", "/a b", "--dry-run"]),
            Ok(Parsed::Run(Command::Install {
                bin_dir: Some(PathBuf::from("/a b")),
                dry_run: true
            }))
        );
        assert_eq!(
            parse_strs(&["install", "--dry-run", "--bin-dir=/x"]),
            Ok(Parsed::Run(Command::Install {
                bin_dir: Some(PathBuf::from("/x")),
                dry_run: true
            }))
        );
    }

    #[test]
    fn install_rejects_bad_options() {
        assert!(parse_strs(&["install", "--bin-dir"]).is_err());
        assert!(parse_strs(&["install", "--bin-dir="]).is_err());
        let error = parse_strs(&["install", "--panel-app", "x"]).unwrap_err();
        assert!(error.0.contains("unknown option `--panel-app`"), "{error}");
        let error = parse_strs(&["install", "extra"]).unwrap_err();
        assert!(error.0.contains("unexpected argument `extra`"), "{error}");
    }

    #[test]
    fn uninstall_and_status() {
        assert_eq!(
            parse_strs(&["uninstall"]),
            Ok(Parsed::Run(Command::Uninstall { dry_run: false }))
        );
        assert_eq!(
            parse_strs(&["uninstall", "--dry-run"]),
            Ok(Parsed::Run(Command::Uninstall { dry_run: true }))
        );
        assert!(parse_strs(&["uninstall", "--keep-defaults"]).is_err());
        assert_eq!(parse_strs(&["status"]), Ok(Parsed::Run(Command::Status)));
        assert!(parse_strs(&["status", "--dry-run"]).is_err());
    }

    #[test]
    fn unknown_commands_and_options() {
        let error = parse_strs(&["start"]).unwrap_err();
        assert!(error.0.contains("unknown command `start`"), "{error}");
        let error = parse_strs(&["--dry-run"]).unwrap_err();
        assert!(error.0.contains("unknown option `--dry-run`"), "{error}");
    }
}
