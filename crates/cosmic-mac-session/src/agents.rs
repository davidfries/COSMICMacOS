// SPDX-License-Identifier: GPL-3.0-or-later
//! The launchd agents cosmicMac installs and their plists.
//!
//! [`AGENTS`] is the single list of agents. Install walks it in order and
//! uninstall in reverse, so later entries may depend on earlier ones (the
//! window manager comes first). Adding the launcher, or a menu-bar helper, is
//! one more entry; an agent shipped as an app bundle names the executable
//! inside it, e.g. `CosmicLauncher.app/Contents/MacOS/cosmic-mac-launcher`.

use std::path::{Path, PathBuf};

use crate::plist::{self, Dict, Value};

/// One launchd agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    /// launchd label; also the plist file name (`<label>.plist`).
    pub label: &'static str,
    /// Executable path relative to `--bin-dir`.
    pub executable: &'static str,
    /// File name in `~/Library/Logs/cosmicMac/` for stdout and stderr.
    pub log_file: &'static str,
    /// Extra environment for the process.
    pub environment: &'static [(&'static str, &'static str)],
}

/// Every agent, in start order.
pub const AGENTS: &[Agent] = &[
    Agent {
        label: "com.cosmicmac.wm",
        executable: "cosmic-mac-wm",
        log_file: "wm.log",
        // cosmic-mac-wm initialises env_logger with `default_filter_or("info")`,
        // so info is already its default; setting RUST_LOG here keeps it at info
        // even if `launchctl setenv RUST_LOG ...` changed the domain default.
        environment: &[("RUST_LOG", "info")],
    },
    // The menu-bar status item. It subscribes to the WM over IPC and retries
    // until the WM is up, so it does not need the WM to start first. Its
    // "Quit" item exits 0, which `KeepAlive { SuccessfulExit = false }` leaves
    // alone until the next login or `install`.
    Agent {
        label: "com.cosmicmac.status",
        executable: "cosmic-mac-status",
        log_file: "status.log",
        environment: &[("RUST_LOG", "info")],
    },
    // The resident launcher, run from its app bundle (bundle identifier and
    // `LSUIElement`: no Dock icon). Started without `--show`, so it waits
    // hidden for `toggle`/`show` on its socket (the WM's Super tap). A second
    // instance exits 0, which `KeepAlive { SuccessfulExit = false }` does
    // not restart.
    Agent {
        label: "com.cosmicmac.launcher",
        executable: "cosmic-mac-launcher.app/Contents/MacOS/cosmic-mac-launcher",
        log_file: "launcher.log",
        // The launcher's own default filter, pinned like the WM's.
        environment: &[("RUST_LOG", "warn,cosmic_mac_launcher=info")],
    },
];

/// Seconds launchd waits before restarting a crashed agent.
pub const THROTTLE_INTERVAL_SECS: i64 = 5;

/// Per-user locations, derived from the home directory (never `$HOME`
/// directly, so tests can use a temporary directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Paths { home: home.into() }
    }

    pub fn launch_agents_dir(&self) -> PathBuf {
        self.home.join("Library/LaunchAgents")
    }

    pub fn plist_path(&self, agent: &Agent) -> PathBuf {
        self.launch_agents_dir()
            .join(format!("{}.plist", agent.label))
    }

    pub fn log_dir(&self) -> PathBuf {
        self.home.join("Library/Logs/cosmicMac")
    }

    pub fn log_path(&self, agent: &Agent) -> PathBuf {
        self.log_dir().join(agent.log_file)
    }
}

impl Agent {
    /// Absolute path of the executable for a given `--bin-dir`.
    pub fn program(&self, bin_dir: &Path) -> PathBuf {
        bin_dir.join(self.executable)
    }

    /// The agent's launchd property list.
    ///
    /// - `KeepAlive { SuccessfulExit = false }`: launchd restarts the agent
    ///   after a crash or non-zero exit, not after a clean exit (the WM exits
    ///   0 after SIGTERM, having restored every window).
    /// - `ThrottleInterval`: at most one restart every few seconds, so a WM
    ///   that exits at once (e.g. Accessibility not granted) does not spin.
    /// - `LimitLoadToSessionType Aqua`: only in the GUI login session.
    pub fn plist(&self, bin_dir: &Path, paths: &Paths) -> Dict {
        let program = path_string(&self.program(bin_dir));
        let log = path_string(&paths.log_path(self));
        let mut dict = Dict::new()
            .with("Label", self.label.into())
            .with("ProgramArguments", Value::Array(vec![program.into()]));
        if !self.environment.is_empty() {
            let environment = self
                .environment
                .iter()
                .fold(Dict::new(), |env, (key, value)| {
                    env.with(key, (*value).into())
                });
            dict = dict.with("EnvironmentVariables", Value::Dict(environment));
        }
        dict.with("RunAtLoad", Value::Bool(true))
            .with(
                "KeepAlive",
                Value::Dict(Dict::new().with("SuccessfulExit", Value::Bool(false))),
            )
            .with("ProcessType", "Interactive".into())
            .with("ThrottleInterval", Value::Integer(THROTTLE_INTERVAL_SECS))
            .with("LimitLoadToSessionType", "Aqua".into())
            .with("StandardOutPath", log.clone().into())
            .with("StandardErrorPath", log.into())
    }

    /// [`Agent::plist`] serialised as XML.
    pub fn plist_xml(&self, bin_dir: &Path, paths: &Paths) -> String {
        plist::to_xml(&self.plist(bin_dir, paths))
    }
}

/// Lossy UTF-8 rendering of a path for the plist (launchd plists are UTF-8).
fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wm() -> &'static Agent {
        AGENTS
            .iter()
            .find(|agent| agent.label == "com.cosmicmac.wm")
            .expect("wm agent")
    }

    #[test]
    fn agent_list_starts_with_the_window_manager_and_labels_are_unique() {
        assert_eq!(AGENTS[0].label, "com.cosmicmac.wm");
        let mut labels: Vec<_> = AGENTS.iter().map(|agent| agent.label).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), AGENTS.len());
        for agent in AGENTS {
            assert!(agent.label.starts_with("com.cosmicmac."));
            assert!(!agent.executable.starts_with('/'));
        }
    }

    fn launcher() -> &'static Agent {
        AGENTS
            .iter()
            .find(|agent| agent.label == "com.cosmicmac.launcher")
            .expect("launcher agent")
    }

    #[test]
    fn launcher_agent_runs_the_bundle_executable_hidden_after_the_wm() {
        let position = |label: &str| AGENTS.iter().position(|agent| agent.label == label);
        assert!(position("com.cosmicmac.wm") < position("com.cosmicmac.launcher"));

        let paths = Paths::new("/Users/someone");
        let dict = launcher().plist(Path::new("/opt/cm/bin"), &paths);
        // No `--show`: launchd starts it hidden.
        assert_eq!(
            dict.get("ProgramArguments"),
            Some(&Value::Array(vec![
                "/opt/cm/bin/cosmic-mac-launcher.app/Contents/MacOS/cosmic-mac-launcher".into()
            ]))
        );
        assert_eq!(
            dict.get("KeepAlive"),
            Some(&Value::Dict(
                Dict::new().with("SuccessfulExit", Value::Bool(false))
            ))
        );
        assert_eq!(dict.get("RunAtLoad"), Some(&Value::Bool(true)));
        assert_eq!(
            dict.get("EnvironmentVariables"),
            Some(&Value::Dict(
                Dict::new().with("RUST_LOG", "warn,cosmic_mac_launcher=info".into())
            ))
        );
        let log: Value = "/Users/someone/Library/Logs/cosmicMac/launcher.log".into();
        assert_eq!(dict.get("StandardOutPath"), Some(&log));
        assert_eq!(dict.get("StandardErrorPath"), Some(&log));
    }

    #[test]
    fn paths_derive_from_home() {
        let paths = Paths::new("/Users/someone");
        assert_eq!(
            paths.plist_path(wm()),
            Path::new("/Users/someone/Library/LaunchAgents/com.cosmicmac.wm.plist")
        );
        assert_eq!(
            paths.log_path(wm()),
            Path::new("/Users/someone/Library/Logs/cosmicMac/wm.log")
        );
        assert_eq!(
            wm().program(Path::new("/opt/cm/bin")),
            Path::new("/opt/cm/bin/cosmic-mac-wm")
        );
    }

    #[test]
    fn wm_plist_has_the_launchd_keys() {
        let paths = Paths::new("/Users/someone");
        let dict = wm().plist(Path::new("/opt/cm/bin"), &paths);
        assert_eq!(
            dict.keys().collect::<Vec<_>>(),
            [
                "Label",
                "ProgramArguments",
                "EnvironmentVariables",
                "RunAtLoad",
                "KeepAlive",
                "ProcessType",
                "ThrottleInterval",
                "LimitLoadToSessionType",
                "StandardOutPath",
                "StandardErrorPath",
            ]
        );
        assert_eq!(dict.get("Label"), Some(&"com.cosmicmac.wm".into()));
        assert_eq!(
            dict.get("ProgramArguments"),
            Some(&Value::Array(vec!["/opt/cm/bin/cosmic-mac-wm".into()]))
        );
        assert_eq!(
            dict.get("EnvironmentVariables"),
            Some(&Value::Dict(Dict::new().with("RUST_LOG", "info".into())))
        );
        assert_eq!(dict.get("RunAtLoad"), Some(&Value::Bool(true)));
        // Restart after a crash, not after a clean (SIGTERM) exit.
        assert_eq!(
            dict.get("KeepAlive"),
            Some(&Value::Dict(
                Dict::new().with("SuccessfulExit", Value::Bool(false))
            ))
        );
        assert_eq!(dict.get("ProcessType"), Some(&"Interactive".into()));
        assert_eq!(dict.get("ThrottleInterval"), Some(&Value::Integer(5)));
        assert_eq!(dict.get("LimitLoadToSessionType"), Some(&"Aqua".into()));
        let log: Value = "/Users/someone/Library/Logs/cosmicMac/wm.log".into();
        assert_eq!(dict.get("StandardOutPath"), Some(&log));
        assert_eq!(dict.get("StandardErrorPath"), Some(&log));
    }

    #[test]
    fn status_item_agent_runs_after_the_window_manager() {
        let position = |label: &str| AGENTS.iter().position(|agent| agent.label == label);
        let status = position("com.cosmicmac.status").expect("status agent");
        assert!(position("com.cosmicmac.wm").unwrap() < status);

        let agent = &AGENTS[status];
        let paths = Paths::new("/Users/someone");
        let dict = agent.plist(Path::new("/opt/cm/bin"), &paths);
        assert_eq!(dict.get("Label"), Some(&"com.cosmicmac.status".into()));
        assert_eq!(
            dict.get("ProgramArguments"),
            Some(&Value::Array(vec!["/opt/cm/bin/cosmic-mac-status".into()]))
        );
        // Restarted after a crash, not after the menu's Quit (exit 0).
        assert_eq!(
            dict.get("KeepAlive"),
            Some(&Value::Dict(
                Dict::new().with("SuccessfulExit", Value::Bool(false))
            ))
        );
        assert_eq!(dict.get("RunAtLoad"), Some(&Value::Bool(true)));
        assert_eq!(dict.get("LimitLoadToSessionType"), Some(&"Aqua".into()));
        let log: Value = "/Users/someone/Library/Logs/cosmicMac/status.log".into();
        assert_eq!(dict.get("StandardOutPath"), Some(&log));
        assert_eq!(dict.get("StandardErrorPath"), Some(&log));
        assert_eq!(
            paths.plist_path(agent),
            Path::new("/Users/someone/Library/LaunchAgents/com.cosmicmac.status.plist")
        );
    }

    #[test]
    fn plist_xml_escapes_paths_with_spaces_and_markup() {
        let paths = Paths::new("/Users/R&D <dev>");
        let xml = wm().plist_xml(Path::new("/Volumes/My Disk/a&b<c>/bin"), &paths);
        assert!(xml.contains(
            "\t<key>ProgramArguments</key>\n\t<array>\n\
             \t\t<string>/Volumes/My Disk/a&amp;b&lt;c&gt;/bin/cosmic-mac-wm</string>\n\
             \t</array>\n"
        ));
        assert!(
            xml.contains(
                "<string>/Users/R&amp;D &lt;dev&gt;/Library/Logs/cosmicMac/wm.log</string>"
            )
        );
        assert!(xml.contains(
            "\t<key>KeepAlive</key>\n\t<dict>\n\
             \t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n"
        ));
        assert!(xml.contains("\t<key>Label</key>\n\t<string>com.cosmicmac.wm</string>\n"));
        assert!(!xml.contains("a&b"));
        assert!(!xml.contains("<dev>"));
    }

    /// The generated plist must be something launchd can read: lint it and
    /// convert it to JSON with the system `plutil` (read-only; macOS only),
    /// then check that escaped paths round-trip.
    #[test]
    #[cfg(target_os = "macos")]
    fn plutil_accepts_the_plist_and_round_trips_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new("/Users/R&D <dev>");
        let bin_dir = Path::new("/Volumes/My Disk/a&b<c>/bin");
        let file = dir.path().join("com.cosmicmac.wm.plist");
        std::fs::write(&file, wm().plist_xml(bin_dir, &paths)).unwrap();

        let lint = std::process::Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&file)
            .output()
            .unwrap();
        assert!(lint.status.success(), "plutil -lint: {lint:?}");

        let program = std::process::Command::new("/usr/bin/plutil")
            .args(["-extract", "ProgramArguments.0", "raw", "-o", "-"])
            .arg(&file)
            .output()
            .unwrap();
        assert!(program.status.success(), "plutil -extract: {program:?}");
        assert_eq!(
            String::from_utf8_lossy(&program.stdout).trim_end(),
            "/Volumes/My Disk/a&b<c>/bin/cosmic-mac-wm"
        );

        let keep_alive = std::process::Command::new("/usr/bin/plutil")
            .args(["-extract", "KeepAlive.SuccessfulExit", "raw", "-o", "-"])
            .arg(&file)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&keep_alive.stdout).trim_end(),
            "false"
        );
    }
}
