// SPDX-License-Identifier: GPL-3.0-or-later
//! External commands (`launchctl`) behind a small trait.
//!
//! Production code spawns real processes through [`SystemRunner`]; unit tests
//! use `FakeRunner`, which records every call and returns scripted outputs, so
//! the install/uninstall logic is tested without touching launchd.

use std::fmt;
use std::io;
use std::process::Command;
use std::time::Duration;

/// One external command: a program and its arguments (no shell involved).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new<I, S>(program: &str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Cmd {
            program: program.to_owned(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// The program followed by its arguments.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

/// Shell-style rendering for humans (and copy-paste): arguments that contain
/// anything beyond a conservative safe set are single-quoted.
impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let words: Vec<String> = self.argv().iter().map(|word| shell_quote(word)).collect();
        f.write_str(&words.join(" "))
    }
}

/// Quotes `word` for display in a POSIX shell.
pub fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:=@,%".contains(c));
    if safe {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// What a finished command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// Exit status; `None` when the process was killed by a signal.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// An exit-0 output with the given stdout.
    pub fn ok(stdout: &str) -> Self {
        Output {
            status: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    /// A failed output with the given exit status and stderr.
    pub fn failed(status: i32, stderr: &str) -> Self {
        Output {
            status: Some(status),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    /// The trimmed stderr, or the exit status when stderr is empty; for
    /// error messages.
    pub fn describe(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_owned();
        }
        match self.status {
            Some(code) => format!("exit status {code}"),
            None => "killed by a signal".to_owned(),
        }
    }
}

/// Runs external commands. The only place that spawns processes.
pub trait CommandRunner {
    /// Runs `cmd` to completion and captures its output. `Err` only when the
    /// process could not be started.
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output>;

    /// Waits between polls (e.g. for launchd to finish stopping a service).
    /// A trait method so tests do not actually sleep.
    fn pause(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Spawns real processes.
#[derive(Debug, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output> {
        let output = Command::new(&cmd.program).args(&cmd.args).output()?;
        Ok(Output {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
pub mod fake {
    //! A scripted [`CommandRunner`] for unit tests.

    use std::collections::{HashMap, VecDeque};

    use super::*;

    /// Records every command and answers from a script.
    ///
    /// Unscripted commands model a machine where nothing is installed:
    /// `launchctl print` fails with launchd's "Could not find service"
    /// (exit 113), everything else succeeds with empty output.
    #[derive(Debug, Default)]
    pub struct FakeRunner {
        /// Every command run, as `argv` joined with single spaces.
        pub calls: Vec<String>,
        /// Number of [`CommandRunner::pause`] calls.
        pub pauses: usize,
        script: HashMap<String, VecDeque<Output>>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        /// Scripts the answers for one command line (`argv` joined with
        /// spaces). Answers are used in order; the last one repeats.
        pub fn respond(&mut self, command_line: &str, outputs: Vec<Output>) {
            assert!(!outputs.is_empty(), "script at least one output");
            self.script
                .insert(command_line.to_owned(), outputs.into_iter().collect());
        }

        fn default_output(command_line: &str) -> Output {
            if command_line.starts_with("launchctl print ") {
                let service = command_line.rsplit('/').next().unwrap_or_default();
                Output::failed(
                    113,
                    &format!(
                        "Bad request.\nCould not find service \"{service}\" in domain for user gui: 501\n"
                    ),
                )
            } else {
                Output::ok("")
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&mut self, cmd: &Cmd) -> io::Result<Output> {
            let line = cmd.argv().join(" ");
            self.calls.push(line.clone());
            let output = match self.script.get_mut(&line) {
                Some(queue) if queue.len() > 1 => queue.pop_front().expect("non-empty"),
                Some(queue) => queue.front().cloned().expect("non-empty"),
                None => Self::default_output(&line),
            };
            Ok(output)
        }

        fn pause(&mut self, _duration: Duration) {
            self.pauses += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_only_unsafe_words() {
        let cmd = Cmd::new(
            "launchctl",
            ["bootstrap", "gui/501", "/Users/a b/it's & <x>.plist"],
        );
        assert_eq!(
            cmd.to_string(),
            "launchctl bootstrap gui/501 '/Users/a b/it'\\''s & <x>.plist'"
        );
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("com.cosmicmac.wm"), "com.cosmicmac.wm");
    }

    #[test]
    fn output_describe_prefers_stderr() {
        assert_eq!(Output::failed(5, "  boom\n").describe(), "boom");
        assert_eq!(Output::failed(5, "").describe(), "exit status 5");
        let signalled = Output {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(signalled.describe(), "killed by a signal");
        assert!(!signalled.success());
        assert!(Output::ok("").success());
    }

    #[test]
    fn system_runner_captures_status_and_output() {
        let mut runner = SystemRunner;
        let output = runner
            .run(&Cmd::new(
                "/bin/sh",
                ["-c", "echo out; echo err >&2; exit 3"],
            ))
            .unwrap();
        assert_eq!(output.status, Some(3));
        assert_eq!(output.stdout, "out\n");
        assert_eq!(output.stderr, "err\n");
        assert!(
            runner
                .run(&Cmd::new("/nonexistent/cosmic-mac-session-test", [""; 0]))
                .is_err()
        );
    }

    #[test]
    fn fake_runner_replays_script_and_repeats_last_answer() {
        let mut fake = fake::FakeRunner::new();
        fake.respond(
            "launchctl print gui/501/x",
            vec![Output::ok("a"), Output::failed(113, "gone")],
        );
        let cmd = Cmd::new("launchctl", ["print", "gui/501/x"]);
        assert!(fake.run(&cmd).unwrap().success());
        assert!(!fake.run(&cmd).unwrap().success());
        assert!(!fake.run(&cmd).unwrap().success());
        // Unscripted print: not loaded; anything else: success.
        let other = Cmd::new("launchctl", ["print", "gui/501/y"]);
        assert_eq!(fake.run(&other).unwrap().status, Some(113));
        assert!(fake.run(&Cmd::new("true", [""; 0])).unwrap().success());
        assert_eq!(fake.calls.len(), 5);
    }
}
