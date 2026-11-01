// SPDX-License-Identifier: GPL-3.0-or-later
//! Applications plugin: scans the standard application folders for `.app`
//! bundles and matches the query against their names.
//!
//! Scanning reads each bundle's `Contents/Info.plist` (XML or binary) with
//! the `plist` crate: the name is `CFBundleDisplayName`, else
//! `CFBundleName`, else the bundle's file name; the file name is also kept
//! as a keyword so "Visual Studio Code" finds the bundle named "Code". Names
//! are not localised (that needs `InfoPlist.strings`/`.loctable` lookups,
//! e.g. through `NSFileManager displayNameAtPath:`).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::matching::{Rank, best_rank};
use crate::plugin::{Action, IconSource, Indice, Plugin, SearchResult};

/// System application folders, in priority order (the first bundle seen for
/// a bundle identifier wins). `~/Applications` is appended by
/// [`default_roots`].
pub const SYSTEM_ROOTS: &[&str] = &[
    "/Applications",
    "/Applications/Utilities",
    "/System/Applications",
    "/System/Applications/Utilities",
];

/// One launchable application bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppEntry {
    /// Name shown in the launcher.
    pub name: String,
    /// `CFBundleIdentifier`, when the bundle declares one.
    pub bundle_id: Option<String>,
    /// Path of the `.app` bundle.
    pub path: PathBuf,
    /// Bundle file name without `.app`, matched like the name.
    pub file_stem: String,
}

/// The folders scanned by default: [`SYSTEM_ROOTS`] plus `~/Applications`.
pub fn default_roots(home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = SYSTEM_ROOTS.iter().map(PathBuf::from).collect();
    if let Some(home) = home {
        roots.push(home.join("Applications"));
    }
    roots
}

/// Scans `roots` for application bundles: every `*.app` directly inside a
/// root, plus every `*.app` one level inside a non-bundle subfolder (e.g.
/// `/Applications/Xcode Tools/Instruments.app`). Missing roots are skipped.
/// Bundles are de-duplicated by canonical path and by bundle identifier
/// (first root wins) and returned sorted by name.
pub fn scan(roots: &[PathBuf]) -> Vec<AppEntry> {
    let mut seen_paths = HashSet::new();
    let mut seen_ids = HashSet::new();
    let mut entries = Vec::new();
    for root in roots {
        for path in bundles_in(root) {
            let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !seen_paths.insert(canonical) {
                continue;
            }
            let Some(entry) = read_bundle(&path) else {
                continue;
            };
            if let Some(id) = &entry.bundle_id
                && !seen_ids.insert(id.to_ascii_lowercase())
            {
                continue;
            }
            entries.push(entry);
        }
    }
    entries.sort_by_cached_key(|entry| entry.name.to_lowercase());
    entries
}

/// `*.app` directories directly in `root` and one level inside its other
/// subfolders.
fn bundles_in(root: &Path) -> Vec<PathBuf> {
    let mut bundles = Vec::new();
    for child in visible_dirs(root) {
        if is_app_bundle(&child) {
            bundles.push(child);
        } else {
            bundles.extend(
                visible_dirs(&child)
                    .into_iter()
                    .filter(|grandchild| is_app_bundle(grandchild)),
            );
        }
    }
    bundles
}

/// Non-hidden directories (following symlinks) in `dir`, sorted by name.
fn visible_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
}

/// Reads one bundle. Bundles without a readable `Info.plist` still appear
/// under their file name (some old or broken bundles lack one); `None` only
/// for paths without a usable file name.
pub fn read_bundle(path: &Path) -> Option<AppEntry> {
    let file_stem = path.file_stem()?.to_string_lossy().into_owned();
    if file_stem.is_empty() {
        return None;
    }
    let info = plist::Value::from_file(path.join("Contents/Info.plist")).ok();
    let info = info.as_ref().and_then(plist::Value::as_dictionary);
    let string = |key: &str| {
        info.and_then(|info| info.get(key))
            .and_then(plist::Value::as_string)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let name = string("CFBundleDisplayName")
        .or_else(|| string("CFBundleName"))
        .unwrap_or_else(|| file_stem.clone());
    Some(AppEntry {
        name,
        bundle_id: string("CFBundleIdentifier"),
        path: path.to_path_buf(),
        file_stem,
    })
}

/// Shortens a bundle path for display: the home directory becomes `~`.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rest) = path.strip_prefix(home)
    {
        return Path::new("~").join(rest).display().to_string();
    }
    path.display().to_string()
}

/// The applications plugin over a scanned list of bundles.
#[derive(Default)]
pub struct AppsPlugin {
    entries: Vec<AppEntry>,
    home: Option<PathBuf>,
    /// Entry index per result id of the latest search.
    last: Vec<usize>,
}

impl AppsPlugin {
    pub fn new(entries: Vec<AppEntry>, home: Option<PathBuf>) -> Self {
        AppsPlugin {
            entries,
            home,
            last: Vec::new(),
        }
    }

    /// Replaces the bundle list (the scan runs in the background at startup).
    pub fn set_entries(&mut self, entries: Vec<AppEntry>) {
        self.entries = entries;
        self.last.clear();
    }
}

impl Plugin for AppsPlugin {
    fn name(&self) -> &'static str {
        "apps"
    }

    fn search(&mut self, query: &str) -> Vec<SearchResult> {
        let mut matches: Vec<(Rank, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                best_rank([entry.name.as_str(), entry.file_stem.as_str()], query)
                    .map(|rank| (rank, index))
            })
            .collect();
        // Entries are sorted by name, so equal ranks stay alphabetical.
        matches.sort_by_key(|&(rank, _)| rank);
        self.last = matches.iter().map(|&(_, index)| index).collect();
        matches
            .into_iter()
            .enumerate()
            .map(|(id, (rank, index))| {
                let entry = &self.entries[index];
                SearchResult {
                    id: id as Indice,
                    name: entry.name.clone(),
                    description: display_path(&entry.path, self.home.as_deref()),
                    icon: Some(IconSource::Bundle(entry.path.clone())),
                    window: None,
                    rank,
                }
            })
            .collect()
    }

    fn activate(&mut self, id: Indice) -> Option<Action> {
        let index = *self.last.get(id as usize)?;
        let entry = self.entries.get(index)?;
        Some(Action::LaunchApp(entry.path.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::Tier;
    use crate::test_support::TempDir;

    /// Writes a fake bundle with an XML Info.plist holding `keys`.
    fn fake_bundle(dir: &Path, relative: &str, keys: &[(&str, &str)]) -> PathBuf {
        let bundle = dir.join(relative);
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        let mut info = plist::Dictionary::new();
        for (key, value) in keys {
            info.insert((*key).to_owned(), plist::Value::String((*value).to_owned()));
        }
        plist::Value::Dictionary(info)
            .to_file_xml(bundle.join("Contents/Info.plist"))
            .unwrap();
        bundle
    }

    fn names(entries: &[AppEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    #[test]
    fn reads_names_with_fallbacks() {
        let tmp = TempDir::new("apps-names");
        let display = fake_bundle(
            tmp.path(),
            "Display.app",
            &[
                ("CFBundleDisplayName", "Shown Name"),
                ("CFBundleName", "Short"),
                ("CFBundleIdentifier", "com.example.display"),
            ],
        );
        let short = fake_bundle(
            tmp.path(),
            "Short.app",
            &[("CFBundleName", "  Bundle Name ")],
        );
        let empty = fake_bundle(tmp.path(), "Fallback.app", &[("CFBundleDisplayName", "")]);
        let bare = tmp.path().join("NoPlist.app");
        fs::create_dir_all(&bare).unwrap();

        let entry = read_bundle(&display).unwrap();
        assert_eq!(entry.name, "Shown Name");
        assert_eq!(entry.bundle_id.as_deref(), Some("com.example.display"));
        assert_eq!(entry.file_stem, "Display");
        assert_eq!(read_bundle(&short).unwrap().name, "Bundle Name");
        assert_eq!(read_bundle(&empty).unwrap().name, "Fallback");
        let bare = read_bundle(&bare).unwrap();
        assert_eq!(bare.name, "NoPlist");
        assert_eq!(bare.bundle_id, None);
    }

    #[test]
    fn reads_binary_plists() {
        let tmp = TempDir::new("apps-binary");
        let bundle = tmp.path().join("Binary.app");
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        let mut info = plist::Dictionary::new();
        info.insert("CFBundleName".into(), "Binary Plist".into());
        plist::Value::Dictionary(info)
            .to_file_binary(bundle.join("Contents/Info.plist"))
            .unwrap();
        assert_eq!(read_bundle(&bundle).unwrap().name, "Binary Plist");
    }

    #[test]
    fn scans_one_level_and_bundles_in_subfolders() {
        let tmp = TempDir::new("apps-scan");
        let root = tmp.path().join("Applications");
        fake_bundle(
            &root,
            "Safari.app",
            &[("CFBundleIdentifier", "com.apple.Safari")],
        );
        fake_bundle(
            &root,
            "Tools/Instruments.app",
            &[("CFBundleName", "Instruments")],
        );
        // Two levels deep inside a folder: not scanned.
        fake_bundle(&root, "Tools/Deeper/Hidden.app", &[]);
        // Bundles nested inside bundles are not apps of their own.
        fake_bundle(&root, "Safari.app/Contents/Helper.app", &[]);
        // Hidden entries and plain files are ignored.
        fake_bundle(&root, ".Secret.app", &[]);
        fs::write(root.join("README.app"), "not a directory").unwrap();

        let entries = scan(&[root.clone(), tmp.path().join("missing")]);
        assert_eq!(names(&entries), ["Instruments", "Safari"]);
        assert_eq!(entries[1].path, root.join("Safari.app"));
    }

    #[test]
    fn deduplicates_by_path_and_bundle_id() {
        let tmp = TempDir::new("apps-dedup");
        let first = tmp.path().join("First");
        let second = tmp.path().join("Second");
        fake_bundle(
            &first,
            "Editor.app",
            &[
                ("CFBundleIdentifier", "com.example.Editor"),
                ("CFBundleName", "Editor"),
            ],
        );
        fake_bundle(
            &second,
            "Editor Copy.app",
            &[
                ("CFBundleIdentifier", "COM.EXAMPLE.EDITOR"),
                ("CFBundleName", "Editor Copy"),
            ],
        );
        fake_bundle(&second, "Other.app", &[]);
        std::os::unix::fs::symlink(second.join("Other.app"), first.join("Other Link.app")).unwrap();

        // `Editor Copy` repeats a bundle id (case-insensitively) and `Other`
        // is the symlink target already seen through `First`.
        let entries = scan(&[first.clone(), second.clone()]);
        assert_eq!(names(&entries), ["Editor", "Other Link"]);
        assert_eq!(entries[0].path, first.join("Editor.app"));
    }

    #[test]
    fn default_roots_include_home_applications() {
        let roots = default_roots(Some(Path::new("/Users/tester")));
        assert_eq!(roots.first(), Some(&PathBuf::from("/Applications")));
        assert_eq!(
            roots.last(),
            Some(&PathBuf::from("/Users/tester/Applications"))
        );
        assert_eq!(roots.len(), SYSTEM_ROOTS.len() + 1);
        assert_eq!(default_roots(None).len(), SYSTEM_ROOTS.len());
    }

    #[test]
    fn display_path_abbreviates_home() {
        let home = Path::new("/Users/tester");
        assert_eq!(
            display_path(Path::new("/Users/tester/Applications/X.app"), Some(home)),
            "~/Applications/X.app"
        );
        assert_eq!(
            display_path(Path::new("/Applications/X.app"), Some(home)),
            "/Applications/X.app"
        );
        assert_eq!(
            display_path(Path::new("/Applications/X.app"), None),
            "/Applications/X.app"
        );
    }

    fn entry(name: &str, stem: &str) -> AppEntry {
        AppEntry {
            name: name.to_owned(),
            bundle_id: None,
            path: PathBuf::from(format!("/Applications/{stem}.app")),
            file_stem: stem.to_owned(),
        }
    }

    #[test]
    fn plugin_ranks_prefix_before_word_prefix_before_substring() {
        let mut entries = vec![
            entry("Activity Monitor", "Activity Monitor"),
            entry("Montage", "Montage"),
            entry("Harmony", "Harmony"),
            entry("Code", "Visual Studio Code"),
        ];
        entries.sort_by_key(|entry| entry.name.to_lowercase());
        let mut apps = AppsPlugin::new(entries, None);

        let results = apps.search("mon");
        let names: Vec<&str> = results.iter().map(|result| result.name.as_str()).collect();
        assert_eq!(names, ["Montage", "Activity Monitor", "Harmony"]);
        assert_eq!(results[0].rank.tier, Tier::Prefix);
        assert_eq!(results[1].rank.tier, Tier::WordPrefix);
        assert_eq!(results[2].rank.tier, Tier::Substring);
        assert_eq!(results[1].description, "/Applications/Activity Monitor.app");

        assert_eq!(
            apps.activate(1),
            Some(Action::LaunchApp(PathBuf::from(
                "/Applications/Activity Monitor.app"
            )))
        );
        assert_eq!(apps.activate(9), None);

        // The bundle file name is a keyword.
        let results = apps.search("visual");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Code");
        assert_eq!(
            results[0].icon,
            Some(IconSource::Bundle(PathBuf::from(
                "/Applications/Visual Studio Code.app"
            )))
        );
    }

    #[test]
    fn replacing_entries_invalidates_old_ids() {
        let mut apps = AppsPlugin::new(vec![entry("Safari", "Safari")], None);
        assert_eq!(apps.search("saf").len(), 1);
        apps.set_entries(vec![entry("Notes", "Notes")]);
        assert_eq!(apps.activate(0), None);
        assert_eq!(apps.search("notes").len(), 1);
    }
}
