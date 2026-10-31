// SPDX-License-Identifier: GPL-3.0-or-later
//! Hand-rolled command-line parsing.
//!
//! A CLI framework would be more code than this parser once the `action <name> [args]`
//! surface is mapped onto [`Action`], and the crate intentionally has no dependencies beyond
//! the protocol and `serde_json`.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use cosmic_mac_protocol::{Action, Direction, WindowId};

use crate::client::parse_timeout_ms;

pub const GLOBAL_HELP: &str = "\
cosmic-mac-ctl — command-line client for the cosmicMac window manager

Usage:
  cosmic-mac-ctl [OPTIONS] <COMMAND> [ARGS]

Options:
  --socket <PATH>      Unix socket to use
                       (default: $COSMIC_MAC_WM_SOCKET, else
                        ~/Library/Application Support/cosmic/cosmic-mac-wm.sock)
  --timeout-ms <MS>    Reply timeout in milliseconds
                       (default: 2000, env COSMIC_MAC_WM_TIMEOUT_MS)
  --json               Machine-readable output (also accepted after the command)
  -h, --help           Show help; use `<COMMAND> --help` for command details
  -V, --version        Show the version

Commands:
  list-windows [--json] [--limit N]   List managed windows
  list-workspaces [--json]            List the workspace pool
  tree [--json] [--workspace N]       Dump the tiling tree
  action [--json] <ACTION> [ARGS]     Perform a window-management action
  focus <WID>                         Focus a window by id
  config [--json]                     Show the layout configuration
  restore                             Unpark windows and restore original frames
  ping                                Check that the daemon is responding

Actions (see `cosmic-mac-ctl action --help`):
  focus <left|right|up|down>          Focus the nearest window in a direction
  focus-window <WID>                  Focus a specific window
  focus-next | focus-previous         Cycle through the focus history
  move <left|right|up|down>           Move the focused window one slot
  swap-window                         Swap with the previously focused window
  toggle-orientation                  Flip the focused group's orientation
  resize <left|right|up|down>         Grow/shrink the focused window
  resize-mode                         Toggle resize mode
  maximize                            Toggle maximize
  toggle-tiling                       Toggle tiling for the workspace
  toggle-window-floating              Float/unfloat the focused window
  toggle-stacking                     Stack with the neighbour / unstack
  unstack                             Remove the focused window from its stack
  workspace <N>                       Switch to workspace N (1-based)
  move-to-workspace <N>               Move the focused window to workspace N
  next-workspace | previous-workspace | last-workspace
  move-to-next-workspace | move-to-previous-workspace | move-to-last-workspace
  close                               Close the focused window

  action --json '<ACTION JSON>'       Send a raw protocol Action object, e.g.
                                      '{\"action\":\"focus\",\"args\":{\"direction\":\"left\"}}'

Exit codes:
  0  success
  1  the daemon reported an error
  2  usage error
  3  IPC error / daemon not running
";

pub const LIST_WINDOWS_HELP: &str = "\
cosmic-mac-ctl list-windows — list managed windows

Usage:
  cosmic-mac-ctl list-windows [--json] [--limit N]

Options:
  --json        Print the exact Response::Windows payload as JSON
  --limit N     Show at most N rows (0 prints just the header)

Columns: WID, PID, APP, TITLE, WS (workspace), STACK, VIS, FLOAT, FRAME.
FRAME is `x,y WxH` in global top-left-origin logical pixels.
";

pub const LIST_WORKSPACES_HELP: &str = "\
cosmic-mac-ctl list-workspaces — list the workspace pool

Usage:
  cosmic-mac-ctl list-workspaces [--json]

Options:
  --json        Print the exact Response::Workspaces payload as JSON

Columns: INDEX, NAME, ACTIVE, WINDOWS.
";

pub const TREE_HELP: &str = "\
cosmic-mac-ctl tree — dump the tiling tree

Usage:
  cosmic-mac-ctl tree [--json] [--workspace N]

Options:
  --json          Print the exact Response::Tree payload as JSON
  --workspace N   Dump workspace N

Note: the IPC `tree` request has no workspace selector and always returns the ACTIVE
workspace.  `--workspace N` therefore first asks for the workspace pool and, when N is not
active, switches to it with `action workspace N` before dumping the tree.  The switch is
reported on stderr.
";

pub const ACTION_HELP: &str = "\
cosmic-mac-ctl action — perform a window-management action

Usage:
  cosmic-mac-ctl action [--json] <ACTION> [ARGS]
  cosmic-mac-ctl action [--json] '<ACTION JSON>'

Directions: left, right, up, down (aliases: h, j, k, l)
Workspace indices are 1-based.

Actions:
  focus <dir>                  Focus the nearest window in a direction
  focus-window <WID>           Focus a specific window (switches workspace if needed)
  focus-next                   Cycle focus forward through the focus history
  focus-previous               Cycle focus backward through the focus history
  move <dir>                   Move the focused window one slot in a direction
  swap-window                  Swap with the previously focused window
  toggle-orientation           Flip the focused window's parent group
  resize <dir>                 Grow towards <dir>, or shrink at the group edge
  resize-mode                  Toggle resize mode
  maximize                     Toggle maximize for the focused window
  toggle-tiling                Toggle tiling for the whole workspace
  toggle-window-floating       Move the focused window between tiling and floating
  toggle-stacking              Add the focused window to its neighbour's stack,
                               or unstack it when already stacked
  unstack                      Remove the focused window from its stack
  workspace <N>                Switch to workspace N, creating it if needed
  move-to-workspace <N>        Move the focused window to workspace N (stay here)
  next-workspace               Switch to the next workspace (wraps)
  previous-workspace           Switch to the previous workspace (wraps)
  last-workspace               Switch back to the previously active workspace
  move-to-next-workspace       Move the focused window to the next workspace
  move-to-previous-workspace   Move the focused window to the previous workspace
  move-to-last-workspace       Move the focused window to the last active workspace
  close                        Close the focused window

Raw JSON:
  The object must be a protocol Action, not a Request:
    cosmic-mac-ctl action --json '{\"action\":\"focus\",\"args\":{\"direction\":\"left\"}}'
    cosmic-mac-ctl action --json '{\"action\":\"workspace\",\"args\":{\"index\":2}}'
";

pub const FOCUS_HELP: &str = "\
cosmic-mac-ctl focus — focus a window by id

Usage:
  cosmic-mac-ctl focus [--json] <WID>

Sends Request::Focus { wid }.  Use `list-windows` to find the id.
";

pub const CONFIG_HELP: &str = "\
cosmic-mac-ctl config — show the layout configuration

Usage:
  cosmic-mac-ctl config [--json]

Options:
  --json        Print the exact Response::Config payload as JSON

Human output covers gaps, active_hint, autotile, tiling_exceptions and passthrough.
";

pub const RESTORE_HELP: &str = "\
cosmic-mac-ctl restore — restore every window's original frame

Usage:
  cosmic-mac-ctl restore

Sends Request::Restore: the daemon unparks windows and replays its frame journal.
";

pub const PING_HELP: &str = "\
cosmic-mac-ctl ping — check that the daemon is responding

Usage:
  cosmic-mac-ctl ping [--json]

The protocol has no dedicated ping request, so this sends `list_windows` and reports the
number of windows.  Exit code 3 means the daemon is not reachable.
";

pub const USAGE_SHORT: &str = "Usage: cosmic-mac-ctl [OPTIONS] <COMMAND> [ARGS]\n\
     Try `cosmic-mac-ctl --help` for more information.";

const COMMANDS: [&str; 8] = [
    "list-windows",
    "list-workspaces",
    "tree",
    "action",
    "focus",
    "config",
    "restore",
    "ping",
];

/// A parsed command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    ListWindows { limit: Option<usize> },
    ListWorkspaces,
    Tree { workspace: Option<u32> },
    Action(Action),
    Focus { wid: WindowId },
    Config,
    Restore,
    Ping,
}

/// A fully parsed invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub command: Command,
    pub json: bool,
    pub socket: Option<PathBuf>,
    pub timeout: Duration,
}

/// Result of parsing the argument vector.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Run(Invocation),
    Help(&'static str),
    Version,
}

/// A usage error (exit code 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

#[derive(Debug, Default)]
struct Stripped {
    json: bool,
    socket: Option<PathBuf>,
    timeout_ms: Option<u64>,
    limit: Option<usize>,
    workspace: Option<u32>,
    raw_json: Option<String>,
    positional: Vec<String>,
    help: bool,
}

/// Parse the process arguments (without `argv[0]`).
pub fn parse<I>(args: I) -> Result<Parsed, UsageError>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let mut socket = None;
    let mut timeout_ms = None;
    let mut json = false;
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--socket" => {
                socket = Some(PathBuf::from(take_value(&args, &mut index, "--socket")?));
            }
            "--timeout-ms" => {
                let value = take_value(&args, &mut index, "--timeout-ms")?;
                timeout_ms = Some(parse_u64(&value, "--timeout-ms")?);
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "-h" | "--help" => return Ok(Parsed::Help(help_topic(&args))),
            "-V" | "--version" => return Ok(Parsed::Version),
            "help" => {
                let topic = args.get(index + 1).map(String::as_str);
                return Ok(Parsed::Help(help_for(topic)));
            }
            _ if arg.starts_with("--socket=") => {
                let value = arg.trim_start_matches("--socket=");
                if value.is_empty() {
                    return Err(UsageError("--socket requires a path".into()));
                }
                socket = Some(PathBuf::from(value));
                index += 1;
            }
            _ if arg.starts_with("--timeout-ms=") => {
                let value = arg.trim_start_matches("--timeout-ms=");
                timeout_ms = Some(parse_u64(value, "--timeout-ms")?);
                index += 1;
            }
            _ if arg.starts_with('-') && arg != "-" => {
                return Err(UsageError(format!(
                    "unknown global option `{arg}`\n{USAGE_SHORT}"
                )));
            }
            _ => break,
        }
    }

    if index >= args.len() {
        return Ok(Parsed::Help(GLOBAL_HELP));
    }

    let name = args[index].clone();
    let rest = &args[index + 1..];
    let parsed = match name.as_str() {
        "list-windows" => parse_list_windows(rest)?,
        "list-workspaces" => {
            parse_unit_command(rest, Command::ListWorkspaces, LIST_WORKSPACES_HELP)?
        }
        "tree" => parse_tree(rest)?,
        "action" => parse_action(rest)?,
        "focus" => parse_focus(rest)?,
        "config" => parse_unit_command(rest, Command::Config, CONFIG_HELP)?,
        "restore" => parse_unit_command(rest, Command::Restore, RESTORE_HELP)?,
        "ping" => parse_unit_command(rest, Command::Ping, PING_HELP)?,
        other => {
            return Err(UsageError(format!(
                "unknown command `{other}`\n{USAGE_SHORT}"
            )));
        }
    };

    if parsed.flags.help {
        return Ok(Parsed::Help(help_for(Some(name.as_str()))));
    }
    let command = parsed
        .command
        .expect("non-help command parse always produces a command");

    json |= parsed.flags.json;
    if parsed.flags.socket.is_some() {
        socket = parsed.flags.socket;
    }
    if parsed.flags.timeout_ms.is_some() {
        timeout_ms = parsed.flags.timeout_ms;
    }

    let env_timeout = std::env::var("COSMIC_MAC_WM_TIMEOUT_MS").ok();
    let timeout = Duration::from_millis(
        parse_timeout_ms(timeout_ms, env_timeout.as_deref()).map_err(UsageError)?,
    );

    Ok(Parsed::Run(Invocation {
        command,
        json,
        socket,
        timeout,
    }))
}

/// A command plus the flags that were mixed into its argument list.
struct ParsedCommand {
    command: Option<Command>,
    flags: Stripped,
}

impl ParsedCommand {
    fn help(flags: Stripped) -> Self {
        ParsedCommand {
            command: None,
            flags,
        }
    }

    fn command(command: Command, flags: Stripped) -> Self {
        ParsedCommand {
            command: Some(command),
            flags,
        }
    }
}

fn parse_list_windows(args: &[String]) -> Result<ParsedCommand, UsageError> {
    let stripped = strip(args, false)?;
    if stripped.help {
        return Ok(ParsedCommand::help(stripped));
    }
    reject_extra(&stripped, "list-windows")?;
    Ok(ParsedCommand::command(
        Command::ListWindows {
            limit: stripped.limit,
        },
        stripped,
    ))
}

fn parse_tree(args: &[String]) -> Result<ParsedCommand, UsageError> {
    let stripped = strip(args, false)?;
    if stripped.help {
        return Ok(ParsedCommand::help(stripped));
    }
    if stripped.limit.is_some() {
        return Err(UsageError("tree does not accept --limit".into()));
    }
    reject_extra(&stripped, "tree")?;
    Ok(ParsedCommand::command(
        Command::Tree {
            workspace: stripped.workspace,
        },
        stripped,
    ))
}

fn parse_focus(args: &[String]) -> Result<ParsedCommand, UsageError> {
    let stripped = strip(args, false)?;
    if stripped.help {
        return Ok(ParsedCommand::help(stripped));
    }
    if stripped.raw_json.is_some() {
        return Err(UsageError("focus does not accept --raw".into()));
    }
    if stripped.workspace.is_some() {
        return Err(UsageError("focus does not accept --workspace".into()));
    }
    if stripped.limit.is_some() {
        return Err(UsageError("focus does not accept --limit".into()));
    }
    if stripped.positional.len() != 1 {
        return Err(UsageError(format!(
            "focus expects exactly one window id\n{FOCUS_HELP}"
        )));
    }
    let wid = parse_wid(&stripped.positional[0])?;
    Ok(ParsedCommand::command(Command::Focus { wid }, stripped))
}

fn parse_unit_command(
    args: &[String],
    command: Command,
    _help: &'static str,
) -> Result<ParsedCommand, UsageError> {
    let stripped = strip(args, false)?;
    if stripped.help {
        return Ok(ParsedCommand::help(stripped));
    }
    reject_extra(&stripped, "this command")?;
    Ok(ParsedCommand::command(command, stripped))
}

fn parse_action(args: &[String]) -> Result<ParsedCommand, UsageError> {
    let stripped = strip(args, true)?;
    if stripped.help {
        return Ok(ParsedCommand::help(stripped));
    }
    if stripped.limit.is_some() {
        return Err(UsageError("action does not accept --limit".into()));
    }
    if stripped.workspace.is_some() {
        return Err(UsageError(
            "action does not accept --workspace; use `action workspace N`".into(),
        ));
    }

    let action = if let Some(raw) = &stripped.raw_json {
        if !stripped.positional.is_empty() {
            return Err(UsageError(
                "raw JSON actions cannot be combined with action arguments".into(),
            ));
        }
        parse_raw_action(raw)?
    } else if stripped.positional.len() == 1 && stripped.positional[0].trim_start().starts_with('{')
    {
        parse_raw_action(&stripped.positional[0])?
    } else {
        parse_friendly_action(&stripped.positional)?
    };

    Ok(ParsedCommand::command(Command::Action(action), stripped))
}

fn parse_raw_action(raw: &str) -> Result<Action, UsageError> {
    serde_json::from_str::<Action>(raw).map_err(|error| {
        UsageError(format!(
            "invalid action JSON: {error}\n  expected e.g. \
             '{{\"action\":\"focus\",\"args\":{{\"direction\":\"left\"}}}}'"
        ))
    })
}

fn parse_friendly_action(tokens: &[String]) -> Result<Action, UsageError> {
    let Some(name) = tokens.first() else {
        return Err(UsageError(format!("missing action name\n{ACTION_HELP}")));
    };
    let name = name.to_ascii_lowercase().replace('_', "-");
    let rest = &tokens[1..];

    let action = match name.as_str() {
        "focus" => Action::Focus {
            direction: expect_direction(rest, "focus")?,
        },
        "focus-window" => Action::FocusWindow {
            wid: expect_wid(rest, "focus-window")?,
        },
        "focus-next" => {
            expect_no_args(rest, "focus-next")?;
            Action::FocusNext
        }
        "focus-previous" => {
            expect_no_args(rest, "focus-previous")?;
            Action::FocusPrevious
        }
        "move" => Action::Move {
            direction: expect_direction(rest, "move")?,
        },
        "swap-window" => {
            expect_no_args(rest, "swap-window")?;
            Action::SwapWindow
        }
        "toggle-orientation" => {
            expect_no_args(rest, "toggle-orientation")?;
            Action::ToggleOrientation
        }
        "resize" => Action::Resize {
            direction: expect_direction(rest, "resize")?,
        },
        "resize-mode" => {
            expect_no_args(rest, "resize-mode")?;
            Action::ResizeMode
        }
        "maximize" => {
            expect_no_args(rest, "maximize")?;
            Action::Maximize
        }
        "toggle-tiling" => {
            expect_no_args(rest, "toggle-tiling")?;
            Action::ToggleTiling
        }
        "toggle-window-floating" | "toggle-floating" => {
            expect_no_args(rest, "toggle-window-floating")?;
            Action::ToggleWindowFloating
        }
        "toggle-stacking" => {
            expect_no_args(rest, "toggle-stacking")?;
            Action::ToggleStacking
        }
        "unstack" => {
            expect_no_args(rest, "unstack")?;
            Action::Unstack
        }
        "workspace" => Action::Workspace {
            index: expect_index(rest, "workspace")?,
        },
        "move-to-workspace" => Action::MoveToWorkspace {
            index: expect_index(rest, "move-to-workspace")?,
        },
        "next-workspace" => {
            expect_no_args(rest, "next-workspace")?;
            Action::NextWorkspace
        }
        "previous-workspace" | "prev-workspace" => {
            expect_no_args(rest, "previous-workspace")?;
            Action::PreviousWorkspace
        }
        "last-workspace" => {
            expect_no_args(rest, "last-workspace")?;
            Action::LastWorkspace
        }
        "move-to-next-workspace" => {
            expect_no_args(rest, "move-to-next-workspace")?;
            Action::MoveToNextWorkspace
        }
        "move-to-previous-workspace" => {
            expect_no_args(rest, "move-to-previous-workspace")?;
            Action::MoveToPreviousWorkspace
        }
        "move-to-last-workspace" => {
            expect_no_args(rest, "move-to-last-workspace")?;
            Action::MoveToLastWorkspace
        }
        "close" => {
            expect_no_args(rest, "close")?;
            Action::Close
        }
        other => {
            return Err(UsageError(format!(
                "unknown action `{other}`\n{ACTION_HELP}"
            )));
        }
    };
    Ok(action)
}

fn expect_direction(args: &[String], action: &str) -> Result<Direction, UsageError> {
    if args.len() != 1 {
        return Err(UsageError(format!(
            "action `{action}` expects exactly one direction (left, right, up, down; h/j/k/l)\n{ACTION_HELP}"
        )));
    }
    parse_direction(&args[0])
}

fn expect_wid(args: &[String], action: &str) -> Result<WindowId, UsageError> {
    if args.len() != 1 {
        return Err(UsageError(format!(
            "action `{action}` expects exactly one window id\n{ACTION_HELP}"
        )));
    }
    parse_wid(&args[0])
}

fn expect_index(args: &[String], action: &str) -> Result<u32, UsageError> {
    if args.len() != 1 {
        return Err(UsageError(format!(
            "action `{action}` expects exactly one 1-based workspace index\n{ACTION_HELP}"
        )));
    }
    parse_index(&args[0])
}

fn expect_no_args(args: &[String], action: &str) -> Result<(), UsageError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(UsageError(format!(
            "action `{action}` takes no arguments (got `{}`)\n{ACTION_HELP}",
            args.join(" ")
        )))
    }
}

fn parse_direction(value: &str) -> Result<Direction, UsageError> {
    match value.to_ascii_lowercase().as_str() {
        "left" | "h" => Ok(Direction::Left),
        "right" | "l" => Ok(Direction::Right),
        "up" | "k" => Ok(Direction::Up),
        "down" | "j" => Ok(Direction::Down),
        other => Err(UsageError(format!(
            "invalid direction `{other}` (expected left, right, up, down; h/j/k/l)"
        ))),
    }
}

fn parse_wid(value: &str) -> Result<WindowId, UsageError> {
    let wid = value.parse::<u64>().map_err(|_| {
        UsageError(format!(
            "invalid window id `{value}` (expected a positive integer)"
        ))
    })?;
    if wid == 0 {
        return Err(UsageError(
            "invalid window id `0` (expected a positive integer)".into(),
        ));
    }
    Ok(wid)
}

fn parse_index(value: &str) -> Result<u32, UsageError> {
    let index = value.parse::<u32>().map_err(|_| {
        UsageError(format!(
            "invalid workspace index `{value}` (expected a 1-based integer)"
        ))
    })?;
    if index == 0 {
        return Err(UsageError(
            "workspace indices are 1-based; `0` is not valid".into(),
        ));
    }
    Ok(index)
}

fn parse_u64(value: &str, flag: &str) -> Result<u64, UsageError> {
    value.parse::<u64>().map_err(|_| {
        UsageError(format!(
            "{flag} expects a non-negative integer, got `{value}`"
        ))
    })
}

fn parse_usize(value: &str, flag: &str) -> Result<usize, UsageError> {
    value.parse::<usize>().map_err(|_| {
        UsageError(format!(
            "{flag} expects a non-negative integer, got `{value}`"
        ))
    })
}

fn reject_extra(stripped: &Stripped, command: &str) -> Result<(), UsageError> {
    if stripped.raw_json.is_some() {
        return Err(UsageError(format!("{command} does not accept --raw")));
    }
    if stripped.workspace.is_some() && command != "tree" {
        return Err(UsageError(format!("{command} does not accept --workspace")));
    }
    if stripped.limit.is_some() && command != "list-windows" {
        return Err(UsageError(format!("{command} does not accept --limit")));
    }
    if !stripped.positional.is_empty() {
        return Err(UsageError(format!(
            "unexpected argument `{}`\n{USAGE_SHORT}",
            stripped.positional.join(" ")
        )));
    }
    Ok(())
}

/// Consume one option value, advancing `index` past both tokens.
fn take_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, UsageError> {
    let value = args
        .get(*index + 1)
        .ok_or_else(|| UsageError(format!("{flag} requires a value\n{USAGE_SHORT}")))?;
    if value.is_empty() {
        return Err(UsageError(format!("{flag} requires a non-empty value")));
    }
    *index += 2;
    Ok(value.clone())
}

fn strip(args: &[String], allow_raw_json: bool) -> Result<Stripped, UsageError> {
    let mut out = Stripped::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => {
                if allow_raw_json
                    && args
                        .get(index + 1)
                        .is_some_and(|next| next.trim_start().starts_with('{'))
                {
                    out.raw_json = Some(args[index + 1].clone());
                    index += 2;
                } else {
                    out.json = true;
                    index += 1;
                }
            }
            "--raw" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| UsageError("--raw requires a JSON object".into()))?;
                out.raw_json = Some(value.clone());
                index += 2;
            }
            "--socket" => {
                out.socket = Some(PathBuf::from(take_value(args, &mut index, "--socket")?));
            }
            "--timeout-ms" => {
                let value = take_value(args, &mut index, "--timeout-ms")?;
                out.timeout_ms = Some(parse_u64(&value, "--timeout-ms")?);
            }
            "--limit" => {
                let value = take_value(args, &mut index, "--limit")?;
                out.limit = Some(parse_usize(&value, "--limit")?);
            }
            "--workspace" => {
                let value = take_value(args, &mut index, "--workspace")?;
                out.workspace = Some(parse_index(&value)?);
            }
            "-h" | "--help" => {
                out.help = true;
                index += 1;
            }
            _ if arg.starts_with("--socket=") => {
                out.socket = Some(PathBuf::from(arg.trim_start_matches("--socket=")));
                index += 1;
            }
            _ if arg.starts_with("--timeout-ms=") => {
                let value = arg.trim_start_matches("--timeout-ms=");
                out.timeout_ms = Some(parse_u64(value, "--timeout-ms")?);
                index += 1;
            }
            _ if arg.starts_with("--limit=") => {
                let value = arg.trim_start_matches("--limit=");
                out.limit = Some(parse_usize(value, "--limit")?);
                index += 1;
            }
            _ if arg.starts_with("--workspace=") => {
                let value = arg.trim_start_matches("--workspace=");
                out.workspace = Some(parse_index(value)?);
                index += 1;
            }
            _ if arg.starts_with('-') && arg != "-" => {
                return Err(UsageError(format!("unknown option `{arg}`\n{USAGE_SHORT}")));
            }
            _ => {
                out.positional.push(args[index].clone());
                index += 1;
            }
        }
    }
    Ok(out)
}

fn help_topic(args: &[String]) -> &'static str {
    args.iter()
        .find(|arg| COMMANDS.contains(&arg.as_str()))
        .map(|arg| help_for(Some(arg)))
        .unwrap_or(GLOBAL_HELP)
}

fn help_for(topic: Option<&str>) -> &'static str {
    match topic {
        Some("list-windows") => LIST_WINDOWS_HELP,
        Some("list-workspaces") => LIST_WORKSPACES_HELP,
        Some("tree") => TREE_HELP,
        Some("action") => ACTION_HELP,
        Some("focus") => FOCUS_HELP,
        Some("config") => CONFIG_HELP,
        Some("restore") => RESTORE_HELP,
        Some("ping") => PING_HELP,
        _ => GLOBAL_HELP,
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Parsed, UsageError, parse};
    use cosmic_mac_protocol::{Action, Direction};
    use std::path::PathBuf;
    use std::time::Duration;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn invocation(values: &[&str]) -> super::Invocation {
        match parse(args(values)).unwrap() {
            Parsed::Run(invocation) => invocation,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    fn action(values: &[&str]) -> Action {
        let invocation = invocation(values);
        match invocation.command {
            Command::Action(action) => action,
            other => panic!("expected Action, got {other:?}"),
        }
    }

    #[test]
    fn every_friendly_action_maps_to_a_protocol_action() {
        let cases: Vec<(Vec<&str>, Action)> = vec![
            (
                vec!["action", "focus", "left"],
                Action::Focus {
                    direction: Direction::Left,
                },
            ),
            (
                vec!["action", "focus", "right"],
                Action::Focus {
                    direction: Direction::Right,
                },
            ),
            (
                vec!["action", "focus", "up"],
                Action::Focus {
                    direction: Direction::Up,
                },
            ),
            (
                vec!["action", "focus", "down"],
                Action::Focus {
                    direction: Direction::Down,
                },
            ),
            (
                vec!["action", "focus-window", "42"],
                Action::FocusWindow { wid: 42 },
            ),
            (vec!["action", "focus-next"], Action::FocusNext),
            (vec!["action", "focus-previous"], Action::FocusPrevious),
            (
                vec!["action", "move", "left"],
                Action::Move {
                    direction: Direction::Left,
                },
            ),
            (vec!["action", "swap-window"], Action::SwapWindow),
            (
                vec!["action", "toggle-orientation"],
                Action::ToggleOrientation,
            ),
            (
                vec!["action", "resize", "down"],
                Action::Resize {
                    direction: Direction::Down,
                },
            ),
            (vec!["action", "resize-mode"], Action::ResizeMode),
            (vec!["action", "maximize"], Action::Maximize),
            (vec!["action", "toggle-tiling"], Action::ToggleTiling),
            (
                vec!["action", "toggle-window-floating"],
                Action::ToggleWindowFloating,
            ),
            (vec!["action", "toggle-stacking"], Action::ToggleStacking),
            (vec!["action", "unstack"], Action::Unstack),
            (
                vec!["action", "workspace", "2"],
                Action::Workspace { index: 2 },
            ),
            (
                vec!["action", "move-to-workspace", "3"],
                Action::MoveToWorkspace { index: 3 },
            ),
            (vec!["action", "next-workspace"], Action::NextWorkspace),
            (
                vec!["action", "previous-workspace"],
                Action::PreviousWorkspace,
            ),
            (vec!["action", "last-workspace"], Action::LastWorkspace),
            (
                vec!["action", "move-to-next-workspace"],
                Action::MoveToNextWorkspace,
            ),
            (
                vec!["action", "move-to-previous-workspace"],
                Action::MoveToPreviousWorkspace,
            ),
            (
                vec!["action", "move-to-last-workspace"],
                Action::MoveToLastWorkspace,
            ),
            (vec!["action", "close"], Action::Close),
        ];
        for (input, expected) in cases {
            assert_eq!(action(&input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn direction_aliases_and_snake_case_actions() {
        assert_eq!(
            action(&["action", "focus", "h"]),
            Action::Focus {
                direction: Direction::Left
            }
        );
        assert_eq!(
            action(&["action", "focus", "l"]),
            Action::Focus {
                direction: Direction::Right
            }
        );
        assert_eq!(
            action(&["action", "focus", "k"]),
            Action::Focus {
                direction: Direction::Up
            }
        );
        assert_eq!(
            action(&["action", "focus", "j"]),
            Action::Focus {
                direction: Direction::Down
            }
        );
        assert_eq!(
            action(&["action", "move_to_workspace", "4"]),
            Action::MoveToWorkspace { index: 4 }
        );
    }

    #[test]
    fn raw_json_action_is_parsed() {
        let raw = r#"{"action":"focus","args":{"direction":"left"}}"#;
        assert_eq!(
            action(&["action", "--json", raw]),
            Action::Focus {
                direction: Direction::Left
            }
        );
        assert_eq!(
            action(&["action", "--raw", raw]),
            Action::Focus {
                direction: Direction::Left
            }
        );
        assert_eq!(
            action(&["action", raw]),
            Action::Focus {
                direction: Direction::Left
            }
        );
    }

    #[test]
    fn action_json_flag_still_means_output_json() {
        let invocation = invocation(&["action", "close", "--json"]);
        assert!(invocation.json);
        assert_eq!(invocation.command, Command::Action(Action::Close));
    }

    #[test]
    fn focus_command_parses_wid() {
        assert_eq!(
            invocation(&["focus", "1234"]).command,
            Command::Focus { wid: 1234 }
        );
    }

    #[test]
    fn tree_workspace_flag_parses() {
        assert_eq!(
            invocation(&["tree", "--workspace", "3"]).command,
            Command::Tree { workspace: Some(3) }
        );
    }

    #[test]
    fn list_windows_limit_parses() {
        assert_eq!(
            invocation(&["list-windows", "--limit", "0"]).command,
            Command::ListWindows { limit: Some(0) }
        );
    }

    #[test]
    fn global_flags_before_and_after_command() {
        let first = invocation(&[
            "--socket",
            "/tmp/a.sock",
            "--timeout-ms",
            "250",
            "list-windows",
            "--json",
        ]);
        assert_eq!(first.socket, Some(PathBuf::from("/tmp/a.sock")));
        assert_eq!(first.timeout, Duration::from_millis(250));
        assert!(first.json);

        let second = invocation(&["list-windows", "--socket=/tmp/b.sock"]);
        assert_eq!(second.socket, Some(PathBuf::from("/tmp/b.sock")));
    }

    #[test]
    fn help_and_version() {
        assert_eq!(
            parse(args(&["--help"])).unwrap(),
            Parsed::Help(super::GLOBAL_HELP)
        );
        assert_eq!(
            parse(args(&["action", "--help"])).unwrap(),
            Parsed::Help(super::ACTION_HELP)
        );
        assert_eq!(
            parse(args(&["tree", "-h"])).unwrap(),
            Parsed::Help(super::TREE_HELP)
        );
        assert_eq!(
            parse(args(&["help", "focus"])).unwrap(),
            Parsed::Help(super::FOCUS_HELP)
        );
        assert_eq!(parse(args(&["--version"])).unwrap(), Parsed::Version);
        assert_eq!(parse(args(&[])).unwrap(), Parsed::Help(super::GLOBAL_HELP));
    }

    #[test]
    fn usage_errors() {
        for input in [
            vec!["nope"],
            vec!["action", "nope"],
            vec!["action", "focus"],
            vec!["action", "focus", "sideways"],
            vec!["action", "workspace", "0"],
            vec!["action", "close", "extra"],
            vec!["focus"],
            vec!["focus", "0"],
            vec!["focus", "abc"],
            vec!["tree", "--workspace", "0"],
            vec!["list-windows", "--limit", "x"],
            vec!["--nope"],
            vec!["action", "--json", "{}"],
        ] {
            let result = parse(args(&input));
            assert!(
                matches!(result, Err(UsageError(_))),
                "expected usage error for {input:?}, got {result:?}"
            );
        }
    }
}
