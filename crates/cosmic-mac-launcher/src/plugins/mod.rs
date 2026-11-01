// SPDX-License-Identifier: GPL-3.0-or-later
//! The launcher's plugins and their priority order.

pub mod apps;
pub mod calc;
pub mod web;
pub mod windows;

use std::path::PathBuf;

use cosmic_mac_protocol::WindowState;

use crate::plugin::{self, Action, Entry, Plugin};

/// Every plugin, searched in priority order: calculator, web search, open
/// windows, applications. Equal ranks keep this order, so an open window
/// of an app comes before the app itself.
#[derive(Default)]
pub struct Plugins {
    pub calc: calc::CalcPlugin,
    pub web: web::WebPlugin,
    pub windows: windows::WindowsPlugin,
    pub apps: apps::AppsPlugin,
}

impl Plugins {
    pub fn new(home: Option<PathBuf>) -> Self {
        Plugins {
            apps: apps::AppsPlugin::new(Vec::new(), home),
            ..Plugins::default()
        }
    }

    fn by_priority(&mut self) -> [&mut dyn Plugin; 4] {
        [
            &mut self.calc,
            &mut self.web,
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

    fn plugins() -> Plugins {
        let mut plugins = Plugins::new(None);
        plugins.set_apps(vec![
            app("Terminal", "com.apple.Terminal"),
            app("TextEdit", "com.apple.TextEdit"),
            app("Calculator", "com.apple.calculator"),
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
    fn limit_applies_to_the_merged_list() {
        let mut plugins = plugins();
        assert_eq!(plugins.search("t", 2).len(), 2);
        assert!(plugins.search("", 8).is_empty());
    }
}
