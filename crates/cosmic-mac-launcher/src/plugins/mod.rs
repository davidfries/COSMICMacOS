// SPDX-License-Identifier: GPL-3.0-or-later
//! The launcher's plugins and their priority order.
//!
//! Query prefixes (pop-launcher's, where it has one; a prefix owns the
//! query, so only that plugin answers):
//!
//! | Prefix              | Plugin   | Does                                        |
//! |---------------------|----------|---------------------------------------------|
//! | `=`                 | calc     | evaluate (also bare `2+2`, without owning)  |
//! | `g `, `ddg `        | web      | Google / DuckDuckGo search                  |
//! | `run `, `t:`        | terminal | run the command in a Terminal.app window    |
//! | `:`                 | terminal | run the command in the background           |
//! | `find `             | files    | Spotlight file-name search in `~`           |
//! | (none)              | windows, apps | WM windows and applications by name    |

pub mod apps;
pub mod calc;
pub mod files;
pub mod terminal;
pub mod web;
pub mod windows;

use std::path::PathBuf;
use std::sync::Arc;

use cosmic_mac_protocol::WindowState;

use crate::plugin::{self, Action, Entry, Plugin};

/// Every plugin, searched in priority order: calculator, web search,
/// terminal, files, open windows, applications. Equal ranks keep this
/// order, so an open window of an app comes before the app itself.
pub struct Plugins {
    pub calc: calc::CalcPlugin,
    pub web: web::WebPlugin,
    pub terminal: terminal::TerminalPlugin,
    pub files: files::FilesPlugin,
    pub windows: windows::WindowsPlugin,
    pub apps: apps::AppsPlugin,
}

impl Plugins {
    /// The plugins with Spotlight (`mdfind`) as the file search.
    pub fn new(home: Option<PathBuf>) -> Self {
        Plugins::with_file_search(home, Arc::new(files::Mdfind::default()))
    }

    pub fn with_file_search(home: Option<PathBuf>, search: Arc<dyn files::FileSearch>) -> Self {
        Plugins {
            calc: calc::CalcPlugin::default(),
            web: web::WebPlugin::default(),
            terminal: terminal::TerminalPlugin::default(),
            files: files::FilesPlugin::new(search, home.clone()),
            windows: windows::WindowsPlugin::default(),
            apps: apps::AppsPlugin::new(Vec::new(), home),
        }
    }

    fn by_priority(&mut self) -> [&mut dyn Plugin; 6] {
        [
            &mut self.calc,
            &mut self.web,
            &mut self.terminal,
            &mut self.files,
            &mut self.windows,
            &mut self.apps,
        ]
    }

    /// Results for `query`, at most `limit`.
    pub fn search(&mut self, query: &str, limit: usize) -> Vec<Entry> {
        plugin::search(&mut self.by_priority(), query, limit)
    }

    /// The action for a result returned by the latest [`Plugins::search`].
    pub fn activate(&mut self, entry: &Entry) -> Option<Action> {
        plugin::activate(&mut self.by_priority(), entry)
    }

    /// Installs the scanned application list; the windows plugin uses it for
    /// app names and icons.
    pub fn set_apps(&mut self, entries: Vec<apps::AppEntry>) {
        self.windows.set_apps(entries.iter().filter_map(|entry| {
            let id = entry.bundle_id.clone()?;
            Some((
                id,
                windows::AppInfo {
                    name: entry.name.clone(),
                    path: entry.path.clone(),
                },
            ))
        }));
        self.apps.set_entries(entries);
    }

    pub fn set_windows(&mut self, windows: Vec<WindowState>) {
        self.windows.set_windows(windows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic_mac_protocol::{Rect, ToplevelInfo};

    fn app(name: &str, id: &str) -> apps::AppEntry {
        apps::AppEntry {
            name: name.to_owned(),
            bundle_id: Some(id.to_owned()),
            path: PathBuf::from(format!("/Applications/{name}.app")),
            file_stem: name.to_owned(),
        }
    }

    fn window(wid: u64, app_id: &str, title: &str) -> WindowState {
        WindowState {
            info: ToplevelInfo::new(wid, app_id, title),
            workspace: 1,
            floating: false,
            visible: true,
            focused: false,
            in_stack: false,
            stack_active: false,
            frame: Rect::new(0.0, 0.0, 10.0, 10.0),
            actual_frame: None,
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry.result.name.as_str())
            .collect()
    }

    /// A file search that finds one fixed path for every name.
    struct OneFile;

    impl files::FileSearch for OneFile {
        fn search(
            &self,
            _name: &str,
            _only_in: Option<&std::path::Path>,
            _limit: usize,
        ) -> Result<Vec<PathBuf>, String> {
            Ok(vec![PathBuf::from(
                "/Users/me/Documents/Terminal notes.txt",
            )])
        }
    }

    fn plugins() -> Plugins {
        let mut plugins =
            Plugins::with_file_search(Some(PathBuf::from("/Users/me")), Arc::new(OneFile));
        plugins.set_apps(vec![
            app("Terminal", "com.apple.Terminal"),
            app("TextEdit", "com.apple.TextEdit"),
            app("Calculator", "com.apple.calculator"),
            app("Find My", "com.apple.findmy"),
            app("Finder", "com.apple.finder"),
        ]);
        plugins.set_windows(vec![window(5, "com.apple.TextEdit", "Notes.txt")]);
        plugins
    }

    #[test]
    fn windows_come_before_their_app_on_equal_rank() {
        let mut plugins = plugins();
        let entries = plugins.search("textedit", 8);
        assert_eq!(names(&entries), ["Notes.txt", "TextEdit"]);
        assert_eq!(entries[0].result.description, "TextEdit · workspace 1");
        assert_eq!(plugins.activate(&entries[0]), Some(Action::FocusWindow(5)));
        assert_eq!(
            plugins.activate(&entries[1]),
            Some(Action::LaunchApp(PathBuf::from(
                "/Applications/TextEdit.app"
            )))
        );
    }

    #[test]
    fn better_matches_win_across_plugins() {
        let mut plugins = plugins();
        // "te": prefix of Terminal and TextEdit, word prefix nowhere else;
        // the window only matches through its app name (also a prefix).
        let entries = plugins.search("te", 8);
        assert_eq!(names(&entries), ["Notes.txt", "Terminal", "TextEdit"]);
    }

    #[test]
    fn calculator_and_web_rows_lead_or_isolate() {
        let mut plugins = plugins();
        let entries = plugins.search("2*3", 8);
        assert_eq!(names(&entries), ["6"]);
        assert_eq!(
            plugins.activate(&entries[0]),
            Some(Action::CopyText("6".into()))
        );

        let entries = plugins.search("g calculator", 8);
        assert_eq!(names(&entries), ["Search Google for “calculator”"]);
        let entries = plugins.search("calc", 8);
        assert_eq!(names(&entries), ["Calculator"]);
    }

    #[test]
    fn terminal_prefixes_own_the_query() {
        let mut plugins = plugins();
        let entries = plugins.search("run top", 8);
        assert_eq!(names(&entries), ["top"]);
        assert_eq!(
            plugins.activate(&entries[0]),
            Some(Action::RunInTerminal("top".into()))
        );
        let entries = plugins.search("t:terminal", 8);
        assert_eq!(names(&entries), ["terminal"]);
        let entries = plugins.search(":say done", 8);
        assert_eq!(
            plugins.activate(&entries[0]),
            Some(Action::RunInBackground("say done".into()))
        );
        // Without a prefix "terminal" is the app.
        assert_eq!(names(&plugins.search("terminal", 8)), ["Terminal"]);
    }

    #[test]
    fn find_prefix_searches_files_in_the_background() {
        let mut plugins = plugins();
        // Apps are suppressed as soon as the prefix is typed, as in
        // pop-launcher ("Find My" needs "find" or "my" alone).
        assert_eq!(names(&plugins.search("find ", 8)), ["Find files"]);
        assert_eq!(
            names(&plugins.search("find terminal", 8)),
            ["Searching for “terminal”…"]
        );
        let query = plugins.files.take_query().expect("a background search");
        let (name, result) = query.run();
        plugins.files.set_results(name, result);
        let entries = plugins.search("find terminal", 8);
        assert_eq!(names(&entries), ["Terminal notes.txt"]);
        assert_eq!(
            plugins.activate(&entries[0]),
            Some(Action::OpenFile(PathBuf::from(
                "/Users/me/Documents/Terminal notes.txt"
            )))
        );
        // No prefix: apps, never files (shorter names first on a tie).
        assert_eq!(names(&plugins.search("find", 8)), ["Finder", "Find My"]);
        assert_eq!(names(&plugins.search("finder", 8)), ["Finder"]);
        assert!(!plugins.files.has_query());
    }

    #[test]
    fn limit_applies_to_the_merged_list() {
        let mut plugins = plugins();
        assert_eq!(plugins.search("t", 2).len(), 2);
        assert!(plugins.search("", 8).is_empty());
    }
}
