// SPDX-License-Identifier: GPL-3.0-or-later
//! Terminal plugin, with pop-launcher's terminal-plugin prefixes:
//!
//! * `run <command>` or `t:<command>`: run the command in a new Terminal.app
//!   window;
//! * `:<command>`: run it in the background, without a terminal.
//!
//! The prefix owns the query (pop-launcher's `isolate`), even before a
//! command is typed.
//!
//! Terminal.app is driven without AppleScript, so no Automation grant is
//! needed: the command goes into a temporary executable `.command` file,
//! which Terminal runs in a new window when the file is opened. The script
//! deletes itself first, runs the command in the user's login shell
//! (`$SHELL -l -c`, so the PATH from `~/.zprofile` applies), and waits for
//! Return before the window may close, like pop-launcher's terminal plugin.
//! Background commands run the same way, detached, with no output.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::matching::Rank;
use crate::plugin::{Action, IconSource, Indice, Plugin, SearchResult};

/// Where a command runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A new Terminal.app window (`run `, `t:`).
    Terminal,
    /// Detached, no terminal (`:`).
    Background,
}

/// Splits `query` into its mode and the trimmed (possibly empty) command.
pub fn parse(query: &str) -> Option<(Mode, &str)> {
    let query = query.trim_start();
    if let Some(command) = query.strip_prefix(':') {
        return Some((Mode::Background, command.trim()));
    }
    if let Some(command) = strip_prefix_ignore_case(query, "t:") {
        return Some((Mode::Terminal, command.trim()));
    }
    strip_prefix_ignore_case(query, "run ").map(|command| (Mode::Terminal, command.trim()))
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// `text` as one single-quoted POSIX shell word.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The user's shell for commands: `$SHELL` when it is an absolute path,
/// else zsh (the macOS default).
pub fn user_shell(shell_env: Option<&str>) -> String {
    match shell_env {
        Some(shell) if shell.starts_with('/') => shell.to_owned(),
        _ => "/bin/zsh".to_owned(),
    }
}

/// The `.command` script that runs `command` in a Terminal window.
pub fn terminal_script(command: &str, shell: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # Written by cosmic-mac-launcher; removes itself.\n\
         rm -f -- \"$0\"\n\
         {shell} -l -c {command}\n\
         status=$?\n\
         printf '\\n[exit status %s] Press Return to close.' \"$status\"\n\
         read -r _\n",
        shell = shell_quote(shell),
        command = shell_quote(command),
    )
}

static SCRIPT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `script` to a new executable, owner-only `.command` file in `dir`
/// (never an existing file, so a planted name in a shared `/tmp` is not
/// followed).
pub fn write_script(dir: &Path, script: &str) -> std::io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    let counter = SCRIPT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "cosmic-launcher-{}-{nanos}-{counter}.command",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)?;
    file.write_all(script.as_bytes())?;
    Ok(path)
}

/// Starts `command` detached in `shell` (own process group, no stdio); a
/// helper thread reaps it.
pub fn spawn_background(command: &str, shell: &str) -> std::io::Result<()> {
    let mut child = Command::new(shell)
        .arg("-l")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    std::thread::Builder::new()
        .name("command-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}

#[derive(Default)]
pub struct TerminalPlugin {
    last: Option<(Mode, String)>,
}

impl Plugin for TerminalPlugin {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn isolates(&self, query: &str) -> bool {
        parse(query).is_some()
    }

    fn search(&mut self, query: &str) -> Vec<SearchResult> {
        self.last = None;
        let Some((mode, command)) = parse(query) else {
            return Vec::new();
        };
        let (name, description) = if command.is_empty() {
            (
                "Run a command".to_owned(),
                "run or t: opens Terminal, : runs it in the background".to_owned(),
            )
        } else {
            self.last = Some((mode, command.to_owned()));
            let description = match mode {
                Mode::Terminal => "Run in Terminal",
                Mode::Background => "Run in the background (no terminal)",
            };
            (command.to_owned(), description.to_owned())
        };
        vec![SearchResult {
            id: 0,
            name,
            description,
            icon: Some(IconSource::Name("utilities-terminal-symbolic")),
            window: None,
            rank: Rank::EXACT,
        }]
    }

    fn activate(&mut self, id: Indice) -> Option<Action> {
        if id != 0 {
            return None;
        }
        let (mode, command) = self.last.clone()?;
        Some(match mode {
            Mode::Terminal => Action::RunInTerminal(command),
            Mode::Background => Action::RunInBackground(command),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn parses_pop_launcher_prefixes() {
        assert_eq!(parse("run ls -la"), Some((Mode::Terminal, "ls -la")));
        assert_eq!(parse("RUN  top "), Some((Mode::Terminal, "top")));
        assert_eq!(parse("t:htop"), Some((Mode::Terminal, "htop")));
        assert_eq!(
            parse("  T: brew update"),
            Some((Mode::Terminal, "brew update"))
        );
        assert_eq!(parse(":open ."), Some((Mode::Background, "open .")));
        assert_eq!(parse(": say hi"), Some((Mode::Background, "say hi")));
        // The prefix alone still owns the query, with an empty command.
        assert_eq!(parse("run "), Some((Mode::Terminal, "")));
        assert_eq!(parse(":"), Some((Mode::Background, "")));
        // Words that merely start like a prefix are not commands.
        assert_eq!(parse("run"), None);
        assert_eq!(parse("runner"), None);
        assert_eq!(parse("terminal"), None);
        assert_eq!(parse("tx: foo"), None);
        assert_eq!(parse("ü"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn quoting_survives_single_quotes() {
        assert_eq!(shell_quote("ls"), "'ls'");
        assert_eq!(shell_quote("echo 'hi'"), r"'echo '\''hi'\'''");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn user_shell_falls_back_to_zsh() {
        assert_eq!(
            user_shell(Some("/opt/homebrew/bin/fish")),
            "/opt/homebrew/bin/fish"
        );
        assert_eq!(user_shell(Some("bash")), "/bin/zsh");
        assert_eq!(user_shell(Some("")), "/bin/zsh");
        assert_eq!(user_shell(None), "/bin/zsh");
    }

    #[test]
    fn script_runs_the_command_in_a_login_shell_and_waits() {
        let script = terminal_script("echo \"it's $HOME\"", "/bin/zsh");
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("rm -f -- \"$0\"\n"));
        assert!(
            script.contains(r#"'/bin/zsh' -l -c 'echo "it'\''s $HOME"'"#),
            "{script}"
        );
        assert!(script.ends_with("read -r _\n"));
    }

    /// The generated script, run by `/bin/sh` with Return on stdin, deletes
    /// itself and executes the command exactly as typed.
    #[test]
    fn script_executes_the_quoted_command_and_removes_itself() {
        let tmp = TempDir::new("term-script");
        let marker = tmp.path().join("marker");
        let command = format!(
            "printf '%s' \"it's\" > {}",
            shell_quote(&marker.to_string_lossy())
        );
        let path = write_script(tmp.path(), &terminal_script(&command, "/bin/sh")).unwrap();
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("command"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        let mut child = Command::new(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "it's");
        assert!(!path.exists(), "the script removes itself");
    }

    #[test]
    fn scripts_never_reuse_a_path() {
        let tmp = TempDir::new("term-unique");
        let a = write_script(tmp.path(), "#!/bin/sh\n").unwrap();
        let b = write_script(tmp.path(), "#!/bin/sh\n").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn background_commands_run_detached() {
        let tmp = TempDir::new("term-bg");
        let partial = tmp.path().join("partial");
        let marker = tmp.path().join("done");
        // `>` creates the file before `echo` writes; `mv` makes it appear
        // complete.
        spawn_background(
            &format!(
                "echo ok > {partial} && mv {partial} {marker}",
                partial = shell_quote(&partial.to_string_lossy()),
                marker = shell_quote(&marker.to_string_lossy()),
            ),
            "/bin/sh",
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ok\n");
    }

    #[test]
    fn plugin_isolates_and_activates() {
        let mut terminal = TerminalPlugin::default();
        assert!(terminal.isolates("run ls"));
        assert!(terminal.isolates(":"));
        assert!(!terminal.isolates("safari"));

        let results = terminal.search("t: ls");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "ls");
        assert_eq!(results[0].description, "Run in Terminal");
        assert_eq!(
            terminal.activate(0),
            Some(Action::RunInTerminal("ls".into()))
        );
        assert_eq!(terminal.activate(1), None);

        terminal.search(":say hi");
        assert_eq!(
            terminal.activate(0),
            Some(Action::RunInBackground("say hi".into()))
        );

        // The bare prefix shows a hint without an action.
        let results = terminal.search("run ");
        assert_eq!(results[0].name, "Run a command");
        assert_eq!(terminal.activate(0), None);
        assert!(terminal.search("safari").is_empty());
        assert_eq!(terminal.activate(0), None);
    }
}
