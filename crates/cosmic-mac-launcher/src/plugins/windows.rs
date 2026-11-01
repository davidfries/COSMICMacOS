// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows plugin: the window manager's managed windows, matched on title
//! and application name; activating one asks the WM to focus it.
//!
//! The list is fetched once per launcher invocation over the WM's IPC
//! socket (`Request::ListWindows`, see `cosmic-mac-protocol`), through
//! `cosmic-mac-ctl`'s client, so `COSMIC_MAC_WM_SOCKET` overrides the path
//! here too. When the WM is not running the plugin simply has no windows.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use cosmic_mac_ctl::client::{Client, resolve_socket_path};
use cosmic_mac_protocol::{Request, Response, WindowId, WindowState};

use crate::matching::{Rank, best_rank};
use crate::plugin::{Action, IconSource, Indice, Plugin, SearchResult};

/// Reply timeout for `ListWindows`; startup must not wait on a hung WM.
pub const LIST_TIMEOUT: Duration = Duration::from_millis(500);
/// Reply timeout for `Focus`.
pub const FOCUS_TIMEOUT: Duration = Duration::from_millis(1000);

/// A client for the default (or `COSMIC_MAC_WM_SOCKET`) socket path.
pub fn default_client(timeout: Duration) -> Result<Client, String> {
    resolve_socket_path(None)
        .map(|path| Client::new(path, timeout))
        .map_err(|error| error.to_string())
}

/// Asks the WM for its managed windows.
pub fn fetch_windows(client: &Client) -> Result<Vec<WindowState>, String> {
    match client
        .request(&Request::ListWindows)
        .map_err(|error| error.to_string())?
        .response
    {
        Response::Windows(windows) => Ok(windows),
        Response::Error(error) => Err(format!("window manager: {error}")),
        other => Err(format!("unexpected reply to list_windows: {other:?}")),
    }
}

/// Asks the WM to focus `wid`.
pub fn focus_window(client: &Client, wid: WindowId) -> Result<(), String> {
    match client
        .request(&Request::Focus { wid })
        .map_err(|error| error.to_string())?
        .response
    {
        Response::Ok => Ok(()),
        Response::Error(error) => Err(format!("window manager: {error}")),
        other => Err(format!("unexpected reply to focus: {other:?}")),
    }
}

/// Name and bundle path of an installed app, keyed by bundle identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppInfo {
    pub name: String,
    pub path: PathBuf,
}

/// Fallback app name from a bundle identifier: its last component
/// (`com.apple.TextEdit` → `TextEdit`).
pub fn name_from_bundle_id(bundle_id: &str) -> &str {
    bundle_id.rsplit('.').next().unwrap_or(bundle_id)
}

#[derive(Default)]
pub struct WindowsPlugin {
    windows: Vec<WindowState>,
    /// Installed apps by lower-cased bundle identifier.
    apps: HashMap<String, AppInfo>,
    /// Window id per result id of the latest search.
    last: Vec<WindowId>,
}

impl WindowsPlugin {
    pub fn set_windows(&mut self, windows: Vec<WindowState>) {
        self.windows = windows;
        self.last.clear();
    }

    /// Supplies app names and icons for the windows' bundle identifiers.
    pub fn set_apps(&mut self, apps: impl IntoIterator<Item = (String, AppInfo)>) {
        self.apps = apps
            .into_iter()
            .map(|(id, info)| (id.to_ascii_lowercase(), info))
            .collect();
    }

    fn app_name<'a>(&'a self, window: &'a WindowState) -> &'a str {
        let app_id = window.info.app_id.as_str();
        match self.apps.get(&app_id.to_ascii_lowercase()) {
            Some(app) => &app.name,
            None => name_from_bundle_id(app_id),
        }
    }
}

impl Plugin for WindowsPlugin {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn search(&mut self, query: &str) -> Vec<SearchResult> {
        let mut matches: Vec<(Rank, &WindowState)> = self
            .windows
            .iter()
            .filter_map(|window| {
                best_rank([window.info.title.as_str(), self.app_name(window)], query)
                    .map(|rank| (rank, window))
            })
            .collect();
        matches.sort_by_key(|&(rank, _)| rank);
        let results: Vec<SearchResult> = matches
            .iter()
            .enumerate()
            .map(|(id, &(rank, window))| {
                let app_name = self.app_name(window);
                let title = if window.info.title.is_empty() {
                    app_name.to_owned()
                } else {
                    window.info.title.clone()
                };
                SearchResult {
                    id: id as Indice,
                    name: title,
                    description: format!("{app_name} · workspace {}", window.workspace),
                    icon: self
                        .apps
                        .get(&window.info.app_id.to_ascii_lowercase())
                        .map(|app| IconSource::Bundle(app.path.clone())),
                    window: Some(window.info.wid),
                    rank,
                }
            })
            .collect();
        self.last = matches.iter().map(|(_, window)| window.info.wid).collect();
        results
    }

    fn activate(&mut self, id: Indice) -> Option<Action> {
        self.last.get(id as usize).copied().map(Action::FocusWindow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use cosmic_mac_protocol::{Rect, ToplevelInfo};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn window(wid: WindowId, app_id: &str, title: &str, workspace: u32) -> WindowState {
        WindowState {
            info: ToplevelInfo::new(wid, app_id, title),
            workspace,
            floating: false,
            visible: true,
            focused: false,
            in_stack: false,
            stack_active: false,
            frame: Rect::new(0.0, 0.0, 100.0, 100.0),
            actual_frame: None,
        }
    }

    /// Serves one connection: records the request line, answers `reply`.
    fn serve_once(reply: Response) -> (TempDir, Client, thread::JoinHandle<String>) {
        let dir = TempDir::new("wm-ipc");
        let path = dir.path().join("wm.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut stream = reader.into_inner();
            let mut json = serde_json::to_string(&reply).unwrap();
            json.push('\n');
            stream.write_all(json.as_bytes()).unwrap();
            line
        });
        (dir, Client::new(path, Duration::from_secs(2)), handle)
    }

    #[test]
    fn fetches_windows_over_the_socket() {
        let windows = vec![window(7, "com.apple.TextEdit", "Notes.txt", 1)];
        let (_dir, client, server) = serve_once(Response::Windows(windows.clone()));
        assert_eq!(fetch_windows(&client).unwrap(), windows);
        assert_eq!(
            server.join().unwrap().trim(),
            r#"{"request":"list_windows"}"#
        );
    }

    #[test]
    fn focus_sends_the_window_id() {
        let (_dir, client, server) = serve_once(Response::Ok);
        focus_window(&client, 42).unwrap();
        assert_eq!(
            server.join().unwrap().trim(),
            r#"{"request":"focus","data":{"wid":42}}"#
        );
    }

    #[test]
    fn daemon_errors_and_odd_replies_are_errors() {
        let (_dir, client, _server) = serve_once(Response::Error("no such window".into()));
        let error = focus_window(&client, 1).unwrap_err();
        assert!(error.contains("no such window"), "{error}");

        let (_dir, client, _server) = serve_once(Response::Ok);
        assert!(fetch_windows(&client).is_err());
    }

    #[test]
    fn missing_daemon_is_an_error_not_a_panic() {
        let dir = TempDir::new("wm-none");
        let client = Client::new(dir.path().join("absent.sock"), LIST_TIMEOUT);
        assert!(fetch_windows(&client).is_err());
    }

    #[test]
    fn bundle_id_fallback_name() {
        assert_eq!(name_from_bundle_id("com.apple.TextEdit"), "TextEdit");
        assert_eq!(name_from_bundle_id("plain"), "plain");
        assert_eq!(name_from_bundle_id(""), "");
    }

    #[test]
    fn matches_title_and_app_name() {
        let mut plugin = WindowsPlugin::default();
        plugin.set_windows(vec![
            window(1, "com.apple.TextEdit", "Shopping list", 1),
            window(2, "com.apple.Terminal", "zsh — 80×24", 2),
            window(3, "com.apple.TextEdit", "", 1),
        ]);
        plugin.set_apps([(
            "com.apple.textedit".to_owned(),
            AppInfo {
                name: "TextEdit".into(),
                path: PathBuf::from("/System/Applications/TextEdit.app"),
            },
        )]);

        // Title match.
        let results = plugin.search("shop");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Shopping list");
        assert_eq!(results[0].description, "TextEdit · workspace 1");
        assert_eq!(results[0].window, Some(1));
        assert_eq!(
            results[0].icon,
            Some(IconSource::Bundle(PathBuf::from(
                "/System/Applications/TextEdit.app"
            )))
        );
        assert_eq!(plugin.activate(0), Some(Action::FocusWindow(1)));

        // App-name match; an untitled window shows the app name.
        let results = plugin.search("textedit");
        let names: Vec<&str> = results.iter().map(|result| result.name.as_str()).collect();
        assert_eq!(names, ["Shopping list", "TextEdit"]);
        assert_eq!(plugin.activate(1), Some(Action::FocusWindow(3)));

        // Unknown bundle ids fall back to the id's last component.
        let results = plugin.search("terminal");
        assert_eq!(results[0].description, "Terminal · workspace 2");
        assert_eq!(results[0].icon, None);

        assert!(plugin.search("nothing here").is_empty());
        assert_eq!(plugin.activate(0), None);
    }
}
