// SPDX-License-Identifier: GPL-3.0-or-later
//! In-process launcher plugins, modelled on pop-launcher's protocol.
//!
//! pop-launcher (github.com/pop-os/launcher) runs each plugin as a separate
//! process that answers `Request::Search(query)` with
//! `PluginResponse::Append(PluginSearchResult)`… `Finished`, and performs
//! `Request::Activate(id)` itself before replying `Close`. Its plugin host
//! (`pop-launcher-service`) only loads such external plugins from Linux
//! paths, and its plugins crate needs Wayland and D-Bus, so the launcher
//! hosts its plugins in-process behind the small [`Plugin`] trait here. The
//! types keep pop-launcher's names and fields where they apply ([`Indice`],
//! [`SearchResult`] ≈ `PluginSearchResult`, [`IconSource`]), so moving to
//! the real protocol later is mechanical.
//!
//! Two deliberate differences, both for host-side unit testing:
//! - plugins return a [`Rank`] with each result; pop-launcher ranks in the
//!   service instead;
//! - activation returns an [`Action`] for the frontend to perform instead
//!   of performing it inside the plugin, so plugins stay free of AppKit and
//!   IPC side effects.

use std::path::PathBuf;

use cosmic_mac_protocol::WindowId;

use crate::matching::Rank;

/// Plugin-local result identifier (pop-launcher's `Indice`).
pub type Indice = u32;

/// Where a result's icon comes from (pop-launcher's `IconSource`, adapted to
/// macOS: app bundles carry their own icons).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IconSource {
    /// A named icon from libcosmic's bundled icon set.
    Name(&'static str),
    /// The icon of the application bundle at this path.
    Bundle(PathBuf),
}

/// One row in the launcher (pop-launcher's `PluginSearchResult`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    /// Identifier passed back to [`Plugin::activate`].
    pub id: Indice,
    /// Title line.
    pub name: String,
    /// Subtitle line.
    pub description: String,
    pub icon: Option<IconSource>,
    /// Set when the result refers to a managed window (pop-launcher's
    /// `window`); the frontend shows the description first for these, like
    /// cosmic-launcher.
    pub window: Option<WindowId>,
    /// Match quality; the service sorts on it, lower first.
    pub rank: Rank,
}

/// Side effect requested by activating a result. Performed by the frontend
/// ([`crate::appkit::perform`]), after which the launcher closes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Launch (or activate) the application bundle at this path.
    LaunchApp(PathBuf),
    /// Open a URL in the default handler.
    OpenUrl(String),
    /// Put text on the general pasteboard.
    CopyText(String),
    /// Ask the window manager to focus a window.
    FocusWindow(WindowId),
}

/// An in-process launcher plugin.
pub trait Plugin {
    /// Short identifier used in logs (pop-launcher's plugin `name`).
    fn name(&self) -> &'static str;

    /// pop-launcher's `query.isolate`: when this returns `true`, the
    /// plugin owns the query and every other plugin's results are dropped
    /// (e.g. `g rust` only searches the web).
    fn isolates(&self, _query: &str) -> bool {
        false
    }

    /// Results for `query`. Ids are only valid until the next `search`.
    fn search(&mut self, query: &str) -> Vec<SearchResult>;

    /// What activating result `id` (from the latest `search`) should do.
    /// `None` means nothing to do; the launcher then stays open.
    fn activate(&mut self, id: Indice) -> Option<Action>;
}

/// A result together with the index of the plugin that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub plugin: usize,
    pub result: SearchResult,
}

/// Runs a query over plugins in priority order (the order of the slice) and
/// merges their results: isolation first, then a stable sort on
/// [`SearchResult::rank`] (so equal ranks keep plugin priority), then the
/// limit.
pub fn search(plugins: &mut [&mut dyn Plugin], query: &str, limit: usize) -> Vec<Entry> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let isolated = plugins.iter().position(|plugin| plugin.isolates(query));
    let mut entries: Vec<Entry> = plugins
        .iter_mut()
        .enumerate()
        .filter(|(index, _)| isolated.is_none_or(|isolated| isolated == *index))
        .flat_map(|(index, plugin)| {
            plugin.search(query).into_iter().map(move |result| Entry {
                plugin: index,
                result,
            })
        })
        .collect();
    // Stable, so equal ranks keep plugin priority and each plugin's order.
    entries.sort_by_key(|entry| entry.result.rank);
    entries.truncate(limit);
    entries
}

/// Activates `entry` through the plugin that produced it.
pub fn activate(plugins: &mut [&mut dyn Plugin], entry: &Entry) -> Option<Action> {
    let plugin = plugins.get_mut(entry.plugin)?;
    let action = plugin.activate(entry.result.id);
    log::debug!(
        "{}: activate {} ({:?}) -> {action:?}",
        plugin.name(),
        entry.result.id,
        entry.result.name
    );
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::Tier;

    /// Returns one result per configured name, ranked as given.
    struct Fake {
        name: &'static str,
        results: Vec<(&'static str, Rank)>,
        isolate_prefix: Option<&'static str>,
        searched: usize,
    }

    impl Fake {
        fn new(name: &'static str, results: Vec<(&'static str, Rank)>) -> Self {
            Fake {
                name,
                results,
                isolate_prefix: None,
                searched: 0,
            }
        }
    }

    impl Plugin for Fake {
        fn name(&self) -> &'static str {
            self.name
        }

        fn isolates(&self, query: &str) -> bool {
            self.isolate_prefix
                .is_some_and(|prefix| query.starts_with(prefix))
        }

        fn search(&mut self, _query: &str) -> Vec<SearchResult> {
            self.searched += 1;
            self.results
                .iter()
                .enumerate()
                .map(|(id, (name, rank))| SearchResult {
                    id: id as Indice,
                    name: (*name).to_owned(),
                    description: String::new(),
                    icon: None,
                    window: None,
                    rank: *rank,
                })
                .collect()
        }

        fn activate(&mut self, id: Indice) -> Option<Action> {
            let (name, _) = self.results.get(id as usize)?;
            Some(Action::OpenUrl(format!("{}:{name}", self.name)))
        }
    }

    fn rank(tier: Tier, position: usize, length: usize) -> Rank {
        Rank {
            tier,
            position,
            length,
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry.result.name.as_str())
            .collect()
    }

    #[test]
    fn merges_by_rank_and_keeps_plugin_priority_on_ties() {
        let mut first = Fake::new(
            "first",
            vec![
                ("first-substring", rank(Tier::Substring, 2, 9)),
                ("first-prefix", rank(Tier::Prefix, 0, 5)),
            ],
        );
        let mut second = Fake::new(
            "second",
            vec![
                ("second-prefix", rank(Tier::Prefix, 0, 5)),
                ("second-exact", Rank::EXACT),
            ],
        );
        let mut plugins: [&mut dyn Plugin; 2] = [&mut first, &mut second];
        let entries = search(&mut plugins, "q", 10);
        assert_eq!(
            names(&entries),
            [
                "second-exact",
                "first-prefix",
                "second-prefix",
                "first-substring"
            ]
        );
        assert_eq!(entries[0].plugin, 1);
    }

    #[test]
    fn limit_truncates_after_sorting() {
        let mut only = Fake::new(
            "only",
            vec![
                ("c", rank(Tier::Fuzzy, 0, 1)),
                ("a", rank(Tier::Prefix, 0, 1)),
                ("b", rank(Tier::WordPrefix, 0, 1)),
            ],
        );
        let mut plugins: [&mut dyn Plugin; 1] = [&mut only];
        assert_eq!(names(&search(&mut plugins, "q", 2)), ["a", "b"]);
    }

    #[test]
    fn isolating_plugin_suppresses_the_others() {
        let mut apps = Fake::new("apps", vec![("app", rank(Tier::Prefix, 0, 3))]);
        let mut web = Fake::new("web", vec![("web", Rank::EXACT)]);
        web.isolate_prefix = Some("g ");
        {
            let mut plugins: [&mut dyn Plugin; 2] = [&mut apps, &mut web];
            assert_eq!(names(&search(&mut plugins, "g rust", 8)), ["web"]);
            assert_eq!(names(&search(&mut plugins, "rust", 8)), ["web", "app"]);
        }
        // The suppressed plugin is not even asked.
        assert_eq!(apps.searched, 1);
    }

    #[test]
    fn empty_query_asks_nobody() {
        let mut apps = Fake::new("apps", vec![("app", rank(Tier::Prefix, 0, 3))]);
        {
            let mut plugins: [&mut dyn Plugin; 1] = [&mut apps];
            assert!(search(&mut plugins, "  ", 8).is_empty());
        }
        assert_eq!(apps.searched, 0);
    }

    #[test]
    fn activate_routes_to_the_owning_plugin() {
        let mut first = Fake::new("first", vec![("one", Rank::EXACT)]);
        let mut second = Fake::new("second", vec![("two", rank(Tier::Prefix, 0, 3))]);
        let mut plugins: [&mut dyn Plugin; 2] = [&mut first, &mut second];
        let entries = search(&mut plugins, "q", 8);
        assert_eq!(
            activate(&mut plugins, &entries[1]),
            Some(Action::OpenUrl("second:two".into()))
        );
        let bogus = Entry {
            plugin: 7,
            result: entries[0].result.clone(),
        };
        assert_eq!(activate(&mut plugins, &bogus), None);
    }
}
