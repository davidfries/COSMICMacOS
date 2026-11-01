// SPDX-License-Identifier: GPL-3.0-or-later
//! `install`, `uninstall` and `status`.
//!
//! Every external effect goes through [`Session`]: commands through its
//! [`CommandRunner`], file changes through its own helpers. In dry-run mode
//! the session prints each change (command, file write, directory creation,
//! file removal) instead of performing it. Read-only queries
//! (`launchctl print`) still run, and are printed, so the dry run shows the
//! plan for this machine, e.g. whether an agent would be booted out first.
//!
//! launchd facts relied on:
//! - `launchctl print gui/<uid>/<label>` exits 0 when the service is loaded
//!   and 113 ("Could not find service") when it is not.
//! - `launchctl bootstrap` fails ("5: Input/output error") when the label is
//!   already loaded, so install boots it out first.
//! - `launchctl bootout` sends SIGTERM (SIGKILL only after the job's
//!   `ExitTimeOut`, 20 s by default). It can return before the process has
//!   exited ("36: Operation now in progress"), so callers poll
//!   `launchctl print` until the service is gone.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agents::{AGENTS, Agent, Paths};
use crate::runner::{Cmd, CommandRunner, Output};

/// How often to poll `launchctl print` while waiting for a booted-out agent
/// to exit.
pub const UNLOAD_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Number of polls before giving up: 30 s, longer than launchd's default
/// 20 s `ExitTimeOut` after which it SIGKILLs the agent anyway.
pub const UNLOAD_POLLS: u32 = 120;

/// Mode for written files. launchd refuses group- or world-writable plists.
const FILE_MODE: u32 = 0o644;

/// A failed operation, with a message for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// What `launchctl print` reports about a loaded service.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceInfo {
    pub state: Option<String>,
    pub pid: Option<u32>,
    pub program: Option<String>,
    pub last_exit_code: Option<String>,
}

impl ServiceInfo {
    /// Parses the top-level (one-tab-indented) properties of
    /// `launchctl print` output; nested blocks are ignored.
    pub fn parse(print_output: &str) -> Self {
        let mut info = ServiceInfo::default();
        for line in print_output.lines() {
            let Some(property) = line.strip_prefix('\t') else {
                continue;
            };
            if property.starts_with('\t') {
                continue;
            }
            let Some((key, value)) = property.split_once(" = ") else {
                continue;
            };
            let value = value.trim().to_owned();
            match key {
                "state" => info.state = Some(value),
                "pid" => info.pid = value.parse().ok(),
                "program" => info.program = Some(value),
                "last exit code" => info.last_exit_code = Some(value),
                _ => {}
            }
        }
        info
    }
}

/// One agent's state, as reported by `status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentStatus {
    pub label: &'static str,
    pub plist_path: PathBuf,
    pub installed: bool,
    pub log_path: PathBuf,
    /// `None` when launchd does not have the service loaded.
    pub service: Option<ServiceInfo>,
}

/// The result of `status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub domain: String,
    pub agents: Vec<AgentStatus>,
}

impl Status {
    /// Human-readable report.
    pub fn render(&self) -> String {
        let mut out = format!("cosmicMac launchd agents ({}):\n", self.domain);
        for agent in &self.agents {
            out.push_str(&format!("\n{}\n", agent.label));
            out.push_str(&format!(
                "  plist:   {} ({})\n",
                agent.plist_path.display(),
                if agent.installed {
                    "installed"
                } else {
                    "not installed"
                }
            ));
            match &agent.service {
                None => out.push_str("  launchd: not loaded\n"),
                Some(info) => {
                    let mut line = String::from("  launchd: loaded");
                    if let Some(state) = &info.state {
                        line.push_str(&format!(", {state}"));
                    }
                    if let Some(pid) = info.pid {
                        line.push_str(&format!(", pid {pid}"));
                    }
                    if let Some(code) = &info.last_exit_code {
                        line.push_str(&format!(", last exit code {code}"));
                    }
                    out.push_str(&line);
                    out.push('\n');
                    if let Some(program) = &info.program {
                        out.push_str(&format!("  program: {program}\n"));
                    }
                }
            }
            out.push_str(&format!("  log:     {}\n", agent.log_path.display()));
        }
        out
    }
}

/// The execution context for one subcommand.
pub struct Session<'a> {
    runner: &'a mut dyn CommandRunner,
    out: &'a mut dyn Write,
    paths: Paths,
    uid: u32,
    dry_run: bool,
}

impl<'a> Session<'a> {
    pub fn new(
        runner: &'a mut dyn CommandRunner,
        out: &'a mut dyn Write,
        paths: Paths,
        uid: u32,
        dry_run: bool,
    ) -> Self {
        Session {
            runner,
            out,
            paths,
            uid,
            dry_run,
        }
    }

    /// `gui/<uid>`: the launchd domain of the user's GUI login session.
    pub fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn service_target(&self, agent: &Agent) -> String {
        format!("gui/{}/{}", self.uid, agent.label)
    }

    /// Writes progress output. A closed stdout must not abort an install
    /// half-way, so write errors are ignored.
    fn say(&mut self, message: impl fmt::Display) {
        let _ = writeln!(self.out, "{message}");
    }

    /// Runs a read-only command, also in dry-run mode.
    fn query(&mut self, cmd: &Cmd) -> Result<Output> {
        if self.dry_run {
            self.say(format_args!("[dry-run] query (runs): {cmd}"));
        }
        self.runner
            .run(cmd)
            .map_err(|error| Error(format!("cannot run `{cmd}`: {error}")))
    }

    /// Runs a command that changes the system; only printed in dry-run mode,
    /// where it "succeeds" with empty output.
    fn change(&mut self, cmd: &Cmd) -> Result<Output> {
        if self.dry_run {
            self.say(format_args!("[dry-run] would run: {cmd}"));
            return Ok(Output::ok(""));
        }
        self.say(format_args!("+ {cmd}"));
        self.runner
            .run(cmd)
            .map_err(|error| Error(format!("cannot run `{cmd}`: {error}")))
    }

    fn create_dir_all(&mut self, dir: &Path) -> Result<()> {
        if dir.is_dir() {
            return Ok(());
        }
        if self.dry_run {
            self.say(format_args!(
                "[dry-run] would create directory {}",
                dir.display()
            ));
            return Ok(());
        }
        self.say(format_args!("+ mkdir -p {}", dir.display()));
        fs::create_dir_all(dir)
            .map_err(|error| Error(format!("cannot create {}: {error}", dir.display())))
    }

    /// Replaces `path` atomically (temporary file in the same directory, then
    /// rename) with mode 0644.
    fn write_file(&mut self, path: &Path, contents: &str) -> Result<()> {
        if self.dry_run {
            self.say(format_args!("[dry-run] would write {}:", path.display()));
            for line in contents.lines() {
                self.say(format_args!("    {line}"));
            }
            return Ok(());
        }
        self.say(format_args!("+ write {}", path.display()));
        write_atomic(path, contents)
            .map_err(|error| Error(format!("cannot write {}: {error}", path.display())))
    }

    fn remove_file(&mut self, path: &Path) -> Result<()> {
        if self.dry_run {
            self.say(format_args!("[dry-run] would remove {}", path.display()));
            return Ok(());
        }
        self.say(format_args!("+ rm {}", path.display()));
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error(format!("cannot remove {}: {error}", path.display()))),
        }
    }

    /// `launchctl print` for the agent; `None` when it is not loaded.
    fn service_info(&mut self, agent: &Agent) -> Result<Option<ServiceInfo>> {
        let target = self.service_target(agent);
        let output = self.query(&Cmd::new("launchctl", ["print", target.as_str()]))?;
        Ok(output.success().then(|| ServiceInfo::parse(&output.stdout)))
    }

    fn is_loaded(&mut self, agent: &Agent) -> Result<bool> {
        Ok(self.service_info(agent)?.is_some())
    }

    /// `launchctl bootout` (SIGTERM) and wait until launchd has dropped the
    /// service. A bootout that reports an error is fine as long as the
    /// service ends up unloaded (it may have exited meanwhile).
    fn bootout(&mut self, agent: &Agent) -> Result<()> {
        let target = self.service_target(agent);
        let output = self.change(&Cmd::new("launchctl", ["bootout", target.as_str()]))?;
        if self.dry_run {
            self.say(format_args!(
                "[dry-run] would wait until {} has exited",
                agent.label
            ));
            return Ok(());
        }
        for _ in 0..UNLOAD_POLLS {
            if !self.is_loaded(agent)? {
                return Ok(());
            }
            self.runner.pause(UNLOAD_POLL_INTERVAL);
        }
        let waited = UNLOAD_POLL_INTERVAL * UNLOAD_POLLS;
        let mut message = format!(
            "{} is still loaded {} s after `launchctl bootout {target}`",
            agent.label,
            waited.as_secs()
        );
        if !output.success() {
            message.push_str(&format!(" (bootout said: {})", output.describe()));
        }
        Err(Error(message))
    }

    /// Writes every agent's plist and (re)loads it, in [`AGENTS`] order.
    /// An agent that is already loaded is booted out first, so running
    /// install again replaces the running agents with the new plists.
    pub fn install(&mut self, bin_dir: &Path) -> Result<()> {
        let domain = self.domain();
        self.say(format_args!(
            "Installing cosmicMac launchd agents into {domain} (programs from {})",
            bin_dir.display()
        ));
        self.check_programs(bin_dir)?;

        let launch_agents_dir = self.paths.launch_agents_dir();
        let log_dir = self.paths.log_dir();
        self.create_dir_all(&launch_agents_dir)?;
        // launchd creates the log files but not their directory.
        self.create_dir_all(&log_dir)?;

        for agent in AGENTS {
            let plist_path = self.paths.plist_path(agent);
            let xml = agent.plist_xml(bin_dir, &self.paths);
            self.write_file(&plist_path, &xml)?;

            if self.is_loaded(agent)? {
                self.say(format_args!(
                    "{} is loaded; booting it out before reloading",
                    agent.label
                ));
                self.bootout(agent)?;
            }

            let plist_arg = plist_path.to_string_lossy().into_owned();
            let output = self.change(&Cmd::new(
                "launchctl",
                ["bootstrap", domain.as_str(), plist_arg.as_str()],
            ))?;
            if !output.success() {
                return Err(Error(format!(
                    "`launchctl bootstrap {domain} {}` failed: {} \
                     (is this user logged in to the GUI session?)",
                    plist_path.display(),
                    output.describe()
                )));
            }
        }

        if self.dry_run {
            self.say("Dry run: nothing was changed.");
            return Ok(());
        }
        self.say(format_args!(
            "Done. Check with `cosmic-mac-session status`; logs in {}.",
            log_dir.display()
        ));
        self.say(
            "cosmic-mac-wm needs Accessibility for the installed binary; without it the \
             agent exits and launchd retries every few seconds (see wm.log).",
        );
        Ok(())
    }

    /// Fails unless every agent's program exists below `bin_dir`. Skipped
    /// (and said so) in dry-run mode.
    fn check_programs(&mut self, bin_dir: &Path) -> Result<()> {
        let programs: Vec<PathBuf> = AGENTS.iter().map(|agent| agent.program(bin_dir)).collect();
        if self.dry_run {
            for program in &programs {
                self.say(format_args!(
                    "[dry-run] not checking that {} exists",
                    program.display()
                ));
            }
            return Ok(());
        }
        let missing: Vec<String> = programs
            .iter()
            .filter(|program| !program.is_file())
            .map(|program| program.display().to_string())
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(Error(format!(
                "missing program(s): {} (deploy them first, or pass --bin-dir)",
                missing.join(", ")
            )))
        }
    }

    /// Boots out and removes every agent, in reverse [`AGENTS`] order. The
    /// window manager gets SIGTERM from launchd and restores all windows
    /// before it exits. Agents that are not loaded or not installed are
    /// skipped; a failure to stop one agent does not keep the others
    /// installed.
    pub fn uninstall(&mut self) -> Result<()> {
        let domain = self.domain();
        self.say(format_args!(
            "Removing cosmicMac launchd agents from {domain}"
        ));
        let mut failures = Vec::new();
        for agent in AGENTS.iter().rev() {
            if self.is_loaded(agent)? {
                if let Err(error) = self.bootout(agent) {
                    failures.push(error.0);
                }
            } else {
                self.say(format_args!("{}: not loaded", agent.label));
            }

            let plist_path = self.paths.plist_path(agent);
            if plist_path.exists() {
                if let Err(error) = self.remove_file(&plist_path) {
                    failures.push(error.0);
                }
            } else {
                self.say(format_args!(
                    "{}: no plist at {}",
                    agent.label,
                    plist_path.display()
                ));
            }
        }
        if failures.is_empty() {
            self.say(if self.dry_run {
                "Dry run: nothing was changed."
            } else {
                "Done."
            });
            Ok(())
        } else {
            Err(Error(failures.join("; ")))
        }
    }

    /// Which agents are installed and loaded.
    pub fn status(&mut self) -> Result<Status> {
        let mut agents = Vec::new();
        for agent in AGENTS {
            let plist_path = self.paths.plist_path(agent);
            agents.push(AgentStatus {
                label: agent.label,
                installed: plist_path.is_file(),
                plist_path,
                log_path: self.paths.log_path(agent),
                service: self.service_info(agent)?,
            });
        }
        Ok(Status {
            domain: self.domain(),
            agents,
        })
    }
}

/// The default `--bin-dir`: the directory of the running executable, with
/// symlinks resolved so the plist names the real file. An explicit
/// directory is made absolute (launchd needs absolute paths) and, when it
/// exists, canonicalised.
pub fn resolve_bin_dir(
    explicit: Option<&Path>,
    current_exe: io::Result<PathBuf>,
) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return fs::canonicalize(dir)
            .or_else(|_| std::path::absolute(dir))
            .map_err(|error| Error(format!("invalid --bin-dir {}: {error}", dir.display())));
    }
    let exe = current_exe.and_then(fs::canonicalize).map_err(|error| {
        Error(format!(
            "cannot locate the running executable ({error}); pass --bin-dir"
        ))
    })?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error(format!("{} has no parent directory", exe.display())))
}

fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let temp = dir.join(format!(
        ".{}.tmp-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = write_then_rename(&temp, path, contents);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_then_rename(temp: &Path, path: &Path, contents: &str) -> io::Result<()> {
    let mut file = fs::File::create(temp)?;
    file.write_all(contents.as_bytes())?;
    file.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
    file.sync_all()?;
    fs::rename(temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::FakeRunner;

    const UID: u32 = 501;

    struct Fixture {
        home: tempfile::TempDir,
        bin: tempfile::TempDir,
        runner: FakeRunner,
        out: Vec<u8>,
    }

    impl Fixture {
        /// A temp home and a bin dir containing every agent's program.
        fn new() -> Self {
            let fixture = Fixture {
                home: tempfile::tempdir().unwrap(),
                bin: tempfile::tempdir().unwrap(),
                runner: FakeRunner::new(),
                out: Vec::new(),
            };
            for agent in AGENTS {
                let program = agent.program(fixture.bin.path());
                fs::create_dir_all(program.parent().unwrap()).unwrap();
                fs::write(&program, "#!/bin/sh\n").unwrap();
            }
            fixture
        }

        fn paths(&self) -> Paths {
            Paths::new(self.home.path())
        }

        fn session(&mut self, dry_run: bool) -> Session<'_> {
            let paths = Paths::new(self.home.path());
            Session::new(&mut self.runner, &mut self.out, paths, UID, dry_run)
        }

        fn output(&self) -> String {
            String::from_utf8_lossy(&self.out).into_owned()
        }

        fn plist(&self, agent: &Agent) -> PathBuf {
            self.paths().plist_path(agent)
        }

        /// Scripts `launchctl print` for `agent` as loaded for the next
        /// `loaded_polls` queries and unloaded afterwards.
        fn loaded_for(&mut self, agent: &Agent, loaded_polls: usize) {
            let mut outputs = vec![Output::ok("\tstate = running\n\tpid = 42\n"); loaded_polls];
            outputs.push(Output::failed(113, "Could not find service"));
            self.runner.respond(
                &format!("launchctl print gui/{UID}/{}", agent.label),
                outputs,
            );
        }
    }

    fn print(agent: &Agent) -> String {
        format!("launchctl print gui/{UID}/{}", agent.label)
    }

    fn bootout(agent: &Agent) -> String {
        format!("launchctl bootout gui/{UID}/{}", agent.label)
    }

    fn bootstrap(paths: &Paths, agent: &Agent) -> String {
        format!(
            "launchctl bootstrap gui/{UID} {}",
            paths.plist_path(agent).display()
        )
    }

    fn launchctl_changes(calls: &[String]) -> Vec<String> {
        calls
            .iter()
            .filter(|call| !call.starts_with("launchctl print "))
            .cloned()
            .collect()
    }

    #[test]
    fn fresh_install_writes_plists_and_bootstraps_in_agent_order() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        fx.session(false).install(&bin).unwrap();

        let paths = fx.paths();
        let mut expected = Vec::new();
        for agent in AGENTS {
            expected.push(print(agent));
            expected.push(bootstrap(&paths, agent));
        }
        assert_eq!(fx.runner.calls, expected);

        for agent in AGENTS {
            let written = fs::read_to_string(fx.plist(agent)).unwrap();
            assert_eq!(written, agent.plist_xml(&bin, &paths));
            let mode = fs::metadata(fx.plist(agent)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o644);
        }
        assert!(paths.log_dir().is_dir());
        // No temporary files left behind.
        let entries: Vec<_> = fs::read_dir(paths.launch_agents_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries.len(), AGENTS.len(), "{entries:?}");
        assert!(fx.output().contains("+ launchctl bootstrap gui/501 "));
    }

    #[test]
    fn wm_is_bootstrapped_before_any_later_agent() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        fx.session(false).install(&bin).unwrap();
        let first_bootstrap = fx
            .runner
            .calls
            .iter()
            .find(|call| call.starts_with("launchctl bootstrap"))
            .unwrap();
        assert!(first_bootstrap.ends_with("/com.cosmicmac.wm.plist"));
    }

    #[test]
    fn reinstall_boots_out_a_loaded_agent_before_bootstrapping() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        let wm = &AGENTS[0];
        // Loaded at the check and on the first poll after bootout, then gone.
        fx.loaded_for(wm, 2);
        fx.session(false).install(&bin).unwrap();

        let paths = fx.paths();
        let calls = &fx.runner.calls;
        assert_eq!(
            &calls[..5],
            [
                print(wm),
                bootout(wm),
                print(wm),
                print(wm),
                bootstrap(&paths, wm),
            ]
        );
        assert_eq!(fx.runner.pauses, 1);
        assert!(fx.output().contains("booting it out before reloading"));
    }

    #[test]
    fn install_twice_is_idempotent() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        fx.session(false).install(&bin).unwrap();
        let first: Vec<String> = AGENTS
            .iter()
            .map(|agent| fs::read_to_string(fx.plist(agent)).unwrap())
            .collect();

        // Second run: every agent is loaded now, and goes away on bootout.
        fx.runner.calls.clear();
        for agent in AGENTS {
            fx.loaded_for(agent, 1);
        }
        fx.session(false).install(&bin).unwrap();
        let second: Vec<String> = AGENTS
            .iter()
            .map(|agent| fs::read_to_string(fx.plist(agent)).unwrap())
            .collect();
        assert_eq!(first, second);

        let paths = fx.paths();
        let mut expected = Vec::new();
        for agent in AGENTS {
            expected.push(bootout(agent));
            expected.push(bootstrap(&paths, agent));
        }
        assert_eq!(launchctl_changes(&fx.runner.calls), expected);
    }

    #[test]
    fn install_fails_when_an_agent_never_unloads() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        let wm = &AGENTS[0];
        fx.runner
            .respond(&print(wm), vec![Output::ok("\tstate = running\n")]);
        fx.runner.respond(
            &bootout(wm),
            vec![Output::failed(
                36,
                "Boot-out failed: 36: Operation now in progress",
            )],
        );
        let error = fx.session(false).install(&bin).unwrap_err();
        assert!(error.0.contains("still loaded 30 s after"), "{error}");
        assert!(error.0.contains("Operation now in progress"), "{error}");
        assert_eq!(fx.runner.pauses, UNLOAD_POLLS as usize);
        assert!(
            !fx.runner
                .calls
                .iter()
                .any(|call| call.starts_with("launchctl bootstrap"))
        );
    }

    #[test]
    fn install_reports_bootstrap_failure() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        let paths = fx.paths();
        fx.runner.respond(
            &bootstrap(&paths, &AGENTS[0]),
            vec![Output::failed(5, "Bootstrap failed: 5: Input/output error")],
        );
        let error = fx.session(false).install(&bin).unwrap_err();
        assert!(error.0.contains("Input/output error"), "{error}");
        assert!(error.0.contains("GUI session"), "{error}");
    }

    #[test]
    fn install_refuses_missing_programs() {
        let mut fx = Fixture::new();
        let empty_bin = tempfile::tempdir().unwrap();
        let error = fx.session(false).install(empty_bin.path()).unwrap_err();
        assert!(error.0.contains("missing program(s)"), "{error}");
        assert!(error.0.contains("cosmic-mac-wm"), "{error}");
        assert!(
            error
                .0
                .contains("cosmic-mac-launcher.app/Contents/MacOS/cosmic-mac-launcher"),
            "{error}"
        );
        assert!(fx.runner.calls.is_empty());
        assert!(!fx.paths().launch_agents_dir().exists());
    }

    #[test]
    fn dry_run_install_performs_nothing() {
        let mut fx = Fixture::new();
        let wm = &AGENTS[0];
        fx.loaded_for(wm, 5);
        let missing_bin = fx.home.path().join("no such dir");
        fx.session(true).install(&missing_bin).unwrap();

        // Only read-only queries reached the runner, and nothing was written.
        assert!(
            fx.runner
                .calls
                .iter()
                .all(|call| call.starts_with("launchctl print ")),
            "{:?}",
            fx.runner.calls
        );
        assert_eq!(fx.runner.pauses, 0);
        assert_eq!(fs::read_dir(fx.home.path()).unwrap().count(), 0);

        let out = fx.output();
        assert!(out.contains("[dry-run] not checking that"), "{out}");
        assert!(
            out.contains(&format!("[dry-run] would run: {}", bootout(wm))),
            "{out}"
        );
        assert!(
            out.contains("[dry-run] would run: launchctl bootstrap gui/501 "),
            "{out}"
        );
        assert!(out.contains("[dry-run] would write "), "{out}");
        assert!(out.contains("    \t<key>Label</key>"), "{out}");
        assert!(out.ends_with("Dry run: nothing was changed.\n"), "{out}");
        assert!(out.contains("[dry-run] would create directory "), "{out}");
        assert!(
            out.contains("[dry-run] query (runs): launchctl print"),
            "{out}"
        );
        assert!(out.contains("/no such dir/cosmic-mac-wm</string>"), "{out}");
        assert!(out.contains("[dry-run] would wait until com.cosmicmac.wm has exited"));
    }

    #[test]
    fn uninstall_boots_out_in_reverse_order_and_removes_plists() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        fx.session(false).install(&bin).unwrap();
        fx.runner.calls.clear();
        for agent in AGENTS {
            fx.loaded_for(agent, 1);
        }

        fx.session(false).uninstall().unwrap();

        let mut expected = Vec::new();
        for agent in AGENTS.iter().rev() {
            expected.push(print(agent));
            expected.push(bootout(agent));
            expected.push(print(agent));
        }
        assert_eq!(fx.runner.calls, expected);
        for agent in AGENTS {
            assert!(!fx.plist(agent).exists());
        }
    }

    #[test]
    fn uninstall_ignores_agents_that_are_not_loaded_or_not_installed() {
        let mut fx = Fixture::new();
        fx.session(false).uninstall().unwrap();
        let expected: Vec<String> = AGENTS.iter().rev().map(print).collect();
        assert_eq!(fx.runner.calls, expected);
        let out = fx.output();
        assert!(out.contains("com.cosmicmac.wm: not loaded"), "{out}");
        assert!(out.contains("com.cosmicmac.wm: no plist at"), "{out}");
    }

    #[test]
    fn uninstall_tolerates_a_failing_bootout_once_the_agent_is_gone() {
        let mut fx = Fixture::new();
        let wm = &AGENTS[0];
        fx.loaded_for(wm, 1);
        fx.runner.respond(
            &bootout(wm),
            vec![Output::failed(3, "Boot-out failed: 3: No such process")],
        );
        fx.session(false).uninstall().unwrap();
    }

    #[test]
    fn uninstall_still_removes_plists_when_an_agent_will_not_stop() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        fx.session(false).install(&bin).unwrap();
        let wm = &AGENTS[0];
        fx.runner
            .respond(&print(wm), vec![Output::ok("\tstate = running\n")]);
        let error = fx.session(false).uninstall().unwrap_err();
        assert!(
            error.0.contains("com.cosmicmac.wm is still loaded"),
            "{error}"
        );
        assert!(!fx.plist(wm).exists());
    }

    #[test]
    fn uninstall_keeps_going_when_a_plist_cannot_be_removed() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        fx.session(false).install(&bin).unwrap();
        // A non-empty directory in place of the wm plist cannot be unlinked.
        let wm = &AGENTS[0];
        fs::remove_file(fx.plist(wm)).unwrap();
        fs::create_dir_all(fx.plist(wm).join("blocker")).unwrap();
        let error = fx.session(false).uninstall().unwrap_err();
        assert!(error.0.contains("cannot remove"), "{error}");
        for agent in &AGENTS[1..] {
            assert!(!fx.plist(agent).exists());
        }
    }

    #[test]
    fn dry_run_uninstall_performs_nothing() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        fx.session(false).install(&bin).unwrap();
        fx.runner.calls.clear();
        for agent in AGENTS {
            fx.loaded_for(agent, 3);
        }
        fx.session(true).uninstall().unwrap();

        assert!(
            fx.runner
                .calls
                .iter()
                .all(|call| call.starts_with("launchctl print ")),
            "{:?}",
            fx.runner.calls
        );
        for agent in AGENTS {
            assert!(fx.plist(agent).exists());
        }
        let out = fx.output();
        assert!(out.contains(&format!("[dry-run] would run: {}", bootout(&AGENTS[0]))));
        assert!(out.contains("[dry-run] would remove "), "{out}");
        assert!(out.ends_with("Dry run: nothing was changed.\n"), "{out}");
    }

    #[test]
    fn status_reports_installed_and_loaded_agents() {
        let mut fx = Fixture::new();
        let bin = fx.bin.path().to_path_buf();
        fx.session(false).install(&bin).unwrap();
        let wm = &AGENTS[0];
        fx.runner.respond(
            &print(wm),
            vec![Output::ok(
                "gui/501/com.cosmicmac.wm = {\n\
                 \tactive count = 1\n\
                 \tpath = /Users/x/Library/LaunchAgents/com.cosmicmac.wm.plist\n\
                 \ttype = LaunchAgent\n\
                 \tstate = running\n\
                 \n\
                 \tprogram = /Users/x/cosmicMac/bin/cosmic-mac-wm\n\
                 \targuments = {\n\
                 \t\t/Users/x/cosmicMac/bin/cosmic-mac-wm\n\
                 \t}\n\
                 \tpid = 4242\n\
                 \tlast exit code = (never exited)\n\
                 \tendpoints = {\n\
                 \t\tstate = nested\n\
                 \t}\n\
                 }\n",
            )],
        );
        let status = fx.session(false).status().unwrap();
        assert_eq!(status.domain, "gui/501");
        let wm_status = &status.agents[0];
        assert!(wm_status.installed);
        assert_eq!(
            wm_status.service,
            Some(ServiceInfo {
                state: Some("running".into()),
                pid: Some(4242),
                program: Some("/Users/x/cosmicMac/bin/cosmic-mac-wm".into()),
                last_exit_code: Some("(never exited)".into()),
            })
        );
        let text = status.render();
        assert!(text.contains("com.cosmicmac.wm\n"), "{text}");
        assert!(text.contains("(installed)"), "{text}");
        assert!(
            text.contains("launchd: loaded, running, pid 4242, last exit code (never exited)"),
            "{text}"
        );
        assert!(
            text.contains("program: /Users/x/cosmicMac/bin/cosmic-mac-wm"),
            "{text}"
        );
    }

    #[test]
    fn status_reports_missing_agents() {
        let mut fx = Fixture::new();
        let status = fx.session(false).status().unwrap();
        assert_eq!(status.agents.len(), AGENTS.len());
        for agent in &status.agents {
            assert!(!agent.installed);
            assert_eq!(agent.service, None);
        }
        let text = status.render();
        assert!(text.contains("(not installed)"), "{text}");
        assert!(text.contains("launchd: not loaded"), "{text}");
    }

    #[test]
    fn resolve_bin_dir_defaults_to_the_executables_directory() {
        let dir = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(dir.path()).unwrap();
        let exe = real.join("cosmic-mac-session");
        fs::write(&exe, "").unwrap();
        let link_dir = tempfile::tempdir().unwrap();
        let link = link_dir.path().join("cosmic-mac-session");
        std::os::unix::fs::symlink(&exe, &link).unwrap();

        assert_eq!(resolve_bin_dir(None, Ok(exe.clone())).unwrap(), real);
        // Symlinks are resolved: the plist names the real binary.
        assert_eq!(resolve_bin_dir(None, Ok(link)).unwrap(), real);
        let error = resolve_bin_dir(None, Err(io::Error::other("nope"))).unwrap_err();
        assert!(error.0.contains("pass --bin-dir"), "{error}");
    }

    #[test]
    fn resolve_bin_dir_makes_explicit_paths_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(dir.path()).unwrap();
        assert_eq!(
            resolve_bin_dir(Some(dir.path()), Err(io::Error::other("unused"))).unwrap(),
            real
        );
        let relative = resolve_bin_dir(
            Some(Path::new("does/not/exist")),
            Err(io::Error::other("unused")),
        )
        .unwrap();
        assert!(relative.is_absolute());
        assert!(relative.ends_with("does/not/exist"));
    }

    #[test]
    fn service_info_ignores_nested_blocks_and_unknown_keys() {
        let info = ServiceInfo::parse("x = {\n\tpid = notanumber\n\t\tstate = nested\n}\n");
        assert_eq!(info, ServiceInfo::default());
    }
}
