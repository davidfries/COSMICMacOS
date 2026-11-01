// SPDX-License-Identifier: GPL-3.0-or-later
//! Files plugin: `find <name>` searches file names with Spotlight, like
//! pop-launcher's `find ` plugin (which owns the query once the prefix is
//! typed). Activating a result opens it in its default app.
//!
//! The search is `mdfind -0 -onlyin ~ -name <name>`: names containing
//! `<name>`, case-insensitively, below the home directory, NUL-separated.
//! Hidden paths and `~/Library` are dropped (pop-launcher's search skips
//! hidden files as well), the rest are ranked by how well the file name
//! matches. `mdfind` takes tens to hundreds of milliseconds, so it never
//! runs inside [`Plugin::search`]: `search` answers from the latest finished
//! search (or shows "Searching…") and leaves a [`FileQuery`] for the
//! frontend, which runs it on a background thread and hands the result back
//! through [`FilesPlugin::set_results`]. The [`FileSearch`] runner is
//! injectable so tests never touch Spotlight.

use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use crate::matching::{Rank, Tier, best_rank};
use crate::plugin::{Action, IconSource, Indice, Plugin, SearchResult};
use crate::plugins::apps::display_path;

/// Paths read from `mdfind` before ranking (the list shows fewer).
pub const READ_LIMIT: usize = 64;
/// Longest a search may take before its partial result is used.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(3);

/// Splits `query` into the trimmed (possibly empty) name after `find `.
pub fn parse(query: &str) -> Option<&str> {
    let query = query.trim_start();
    let head = query.get(..5)?;
    head.eq_ignore_ascii_case("find ")
        .then(|| query[5..].trim())
}

/// Runs a file-name search. Implementations must be callable from any
/// thread.
pub trait FileSearch: Send + Sync {
    /// Up to `limit` paths whose names contain `name`, below `only_in` when
    /// given.
    fn search(
        &self,
        name: &str,
        only_in: Option<&Path>,
        limit: usize,
    ) -> Result<Vec<PathBuf>, String>;
}

/// Spotlight through `mdfind`.
pub struct Mdfind {
    pub program: PathBuf,
    pub timeout: Duration,
}

impl Default for Mdfind {
    fn default() -> Self {
        Mdfind {
            program: PathBuf::from("/usr/bin/mdfind"),
            timeout: SEARCH_TIMEOUT,
        }
    }
}

/// `mdfind` arguments for a name search.
pub fn mdfind_args(name: &str, only_in: Option<&Path>) -> Vec<String> {
    let mut args = vec!["-0".to_owned()];
    if let Some(dir) = only_in {
        args.push("-onlyin".to_owned());
        args.push(dir.to_string_lossy().into_owned());
    }
    args.push("-name".to_owned());
    args.push(name.to_owned());
    args
}

impl FileSearch for Mdfind {
    fn search(
        &self,
        name: &str,
        only_in: Option<&Path>,
        limit: usize,
    ) -> Result<Vec<PathBuf>, String> {
        let mut child = Command::new(&self.program)
            .args(mdfind_args(name, only_in))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot run {}: {error}", self.program.display()))?;
        let stdout = child.stdout.take().expect("piped stdout");
        // Read on a helper thread so a slow search can be cut off.
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for entry in BufReader::new(stdout).split(b'\0') {
                let Ok(bytes) = entry else { break };
                if sender.send(bytes).is_err() {
                    break;
                }
            }
        });
        let deadline = std::time::Instant::now() + self.timeout;
        let mut paths = Vec::new();
        let mut timed_out = false;
        while paths.len() < limit {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match receiver.recv_timeout(left) {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    let text = text.trim_matches('\n');
                    if !text.is_empty() {
                        paths.push(PathBuf::from(text));
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    timed_out = true;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Enough results, a timeout, or the end: never leave it running.
        let _ = child.kill();
        let _ = child.wait();
        if timed_out && paths.is_empty() {
            return Err("Spotlight did not answer in time".to_owned());
        }
        Ok(paths)
    }
}

/// Whether `path` should be listed: not hidden, not in `~/Library`.
pub fn is_listed(path: &Path, home: Option<&Path>) -> bool {
    if home.is_some_and(|home| path.starts_with(home.join("Library"))) {
        return false;
    }
    !path.components().any(|component| match component {
        Component::Normal(name) => name.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

/// A search for the frontend to run off the UI thread.
pub struct FileQuery {
    pub name: String,
    runner: Arc<dyn FileSearch>,
    home: Option<PathBuf>,
}

impl FileQuery {
    /// Runs the search (blocking) and filters the paths.
    pub fn run(self) -> (String, Result<Vec<PathBuf>, String>) {
        let result = self
            .runner
            .search(&self.name, self.home.as_deref(), READ_LIMIT)
            .map(|paths| {
                paths
                    .into_iter()
                    .filter(|path| is_listed(path, self.home.as_deref()))
                    .collect()
            });
        (self.name, result)
    }
}

pub struct FilesPlugin {
    runner: Arc<dyn FileSearch>,
    home: Option<PathBuf>,
    /// The latest finished search.
    results: Option<(String, Result<Vec<PathBuf>, String>)>,
    /// The name the latest query asks for, while it has no results.
    wanted: Option<String>,
    /// The name handed out with [`FilesPlugin::take_query`] and not back yet.
    running: Option<String>,
    /// Path per result id of the latest search.
    last: Vec<PathBuf>,
}

impl FilesPlugin {
    pub fn new(runner: Arc<dyn FileSearch>, home: Option<PathBuf>) -> Self {
        FilesPlugin {
            runner,
            home,
            results: None,
            wanted: None,
            running: None,
            last: Vec::new(),
        }
    }

    /// Whether the latest query waits for a search nobody runs yet.
    pub fn has_query(&self) -> bool {
        self.wanted.is_some() && self.wanted != self.running
    }

    /// The search the latest query needs, once per name.
    pub fn take_query(&mut self) -> Option<FileQuery> {
        if !self.has_query() {
            return None;
        }
        let name = self.wanted.clone()?;
        self.running = Some(name.clone());
        Some(FileQuery {
            name,
            runner: self.runner.clone(),
            home: self.home.clone(),
        })
    }

    /// Installs a finished search (the frontend then searches again).
    pub fn set_results(&mut self, name: String, result: Result<Vec<PathBuf>, String>) {
        if self.running.as_deref() == Some(name.as_str()) {
            self.running = None;
        }
        // The next `search` shows them; nothing is pending for this name.
        if self.wanted.as_deref() == Some(name.as_str()) {
            self.wanted = None;
        }
        self.results = Some((name, result));
    }

    /// Forgets results and pending searches (the launcher was hidden).
    pub fn clear(&mut self) {
        self.results = None;
        self.wanted = None;
        self.running = None;
        self.last.clear();
    }

    fn hint(name: &str, description: String) -> SearchResult {
        SearchResult {
            id: 0,
            name: name.to_owned(),
            description,
            icon: Some(IconSource::Name("system-search-symbolic")),
            window: None,
            rank: Rank::EXACT,
        }
    }
}

impl Plugin for FilesPlugin {
    fn name(&self) -> &'static str {
        "files"
    }

    fn isolates(&self, query: &str) -> bool {
        parse(query).is_some()
    }

    fn search(&mut self, query: &str) -> Vec<SearchResult> {
        self.last.clear();
        let Some(name) = parse(query) else {
            self.wanted = None;
            return Vec::new();
        };
        if name.is_empty() {
            self.wanted = None;
            return vec![Self::hint(
                "Find files",
                "Type a file name after “find ” to search with Spotlight".to_owned(),
            )];
        }
        let paths = match &self.results {
            Some((done, Ok(paths))) if done == name => paths.clone(),
            Some((done, Err(error))) if done == name => {
                return vec![Self::hint("File search failed", error.clone())];
            }
            _ => {
                self.wanted = Some(name.to_owned());
                return vec![Self::hint(
                    &format!("Searching for “{name}”…"),
                    "Spotlight file names in your home folder".to_owned(),
                )];
            }
        };
        self.wanted = None;
        if paths.is_empty() {
            return vec![Self::hint(
                &format!("No files named like “{name}”"),
                "Spotlight file names in your home folder".to_owned(),
            )];
        }
        let mut ranked: Vec<(Rank, PathBuf)> = paths
            .into_iter()
            .map(|path| {
                let file_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // Spotlight may match a display name or another spelling.
                let rank = best_rank([file_name.as_str()], name).unwrap_or(Rank {
                    tier: Tier::Fuzzy,
                    position: usize::MAX,
                    length: file_name.chars().count(),
                });
                (rank, path)
            })
            .collect();
        // Stable: equal ranks keep Spotlight's order.
        ranked.sort_by_key(|(rank, _)| *rank);
        let results = ranked
            .iter()
            .enumerate()
            .map(|(id, (rank, path))| SearchResult {
                id: id as Indice,
                name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string()),
                description: path
                    .parent()
                    .map(|parent| display_path(parent, self.home.as_deref()))
                    .unwrap_or_default(),
                icon: Some(IconSource::File(path.clone())),
                window: None,
                rank: *rank,
            })
            .collect();
        self.last = ranked.into_iter().map(|(_, path)| path).collect();
        results
    }

    fn activate(&mut self, id: Indice) -> Option<Action> {
        self.last.get(id as usize).cloned().map(Action::OpenFile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::sync::Mutex;

    /// Returns canned paths and records every call.
    #[derive(Default)]
    struct FakeSearch {
        paths: Vec<PathBuf>,
        calls: Mutex<Vec<(String, Option<PathBuf>, usize)>>,
    }

    impl FileSearch for FakeSearch {
        fn search(
            &self,
            name: &str,
            only_in: Option<&Path>,
            limit: usize,
        ) -> Result<Vec<PathBuf>, String> {
            self.calls.lock().unwrap().push((
                name.to_owned(),
                only_in.map(Path::to_path_buf),
                limit,
            ));
            Ok(self.paths.clone())
        }
    }

    fn plugin(paths: &[&str]) -> (FilesPlugin, Arc<FakeSearch>) {
        let fake = Arc::new(FakeSearch {
            paths: paths.iter().map(PathBuf::from).collect(),
            ..FakeSearch::default()
        });
        let plugin = FilesPlugin::new(fake.clone(), Some(PathBuf::from("/Users/me")));
        (plugin, fake)
    }

    fn names(results: &[SearchResult]) -> Vec<&str> {
        results.iter().map(|result| result.name.as_str()).collect()
    }

    #[test]
    fn parses_the_find_prefix() {
        assert_eq!(parse("find report"), Some("report"));
        assert_eq!(parse("  FIND  my notes "), Some("my notes"));
        assert_eq!(parse("find "), Some(""));
        assert_eq!(parse("find"), None);
        assert_eq!(parse("finder"), None);
        assert_eq!(parse("Find My"), Some("My"));
        assert_eq!(parse("fünf x"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn mdfind_arguments() {
        assert_eq!(
            mdfind_args("report", Some(Path::new("/Users/me"))),
            ["-0", "-onlyin", "/Users/me", "-name", "report"]
        );
        assert_eq!(mdfind_args("a b", None), ["-0", "-name", "a b"]);
    }

    #[test]
    fn hidden_paths_and_library_are_not_listed() {
        let home = Some(Path::new("/Users/me"));
        assert!(is_listed(Path::new("/Users/me/Documents/report.pdf"), home));
        assert!(!is_listed(
            Path::new("/Users/me/Library/Caches/report.txt"),
            home
        ));
        assert!(!is_listed(Path::new("/Users/me/.config/report"), home));
        assert!(!is_listed(Path::new("/Users/me/src/.git/report"), home));
        assert!(is_listed(Path::new("/Users/me/LibraryNotes/report"), home));
        assert!(is_listed(Path::new("/Volumes/USB/report"), None));
    }

    #[test]
    fn a_query_asks_for_one_background_search_then_shows_ranked_results() {
        let (mut files, fake) = plugin(&[
            "/Users/me/Archive/oldreport.txt",
            "/Users/me/Documents/report.pdf",
            "/Users/me/.hidden/report.md",
            "/Users/me/Library/Mail/report.eml",
            "/Users/me/Desktop/Quarterly Report.key",
        ]);
        assert!(files.isolates("find report"));
        assert!(!files.isolates("report"));

        let pending = files.search("find report");
        assert_eq!(names(&pending), ["Searching for “report”…"]);
        assert_eq!(files.activate(0), None);
        assert!(files.has_query());
        let query = files.take_query().expect("a search to run");
        // Asked once, even if the query is searched again meanwhile.
        files.search("find report");
        assert!(files.take_query().is_none());

        let (name, result) = query.run();
        assert_eq!(name, "report");
        assert_eq!(
            *fake.calls.lock().unwrap(),
            [(
                "report".to_owned(),
                Some(PathBuf::from("/Users/me")),
                READ_LIMIT
            )]
        );
        files.set_results(name, result);
        assert!(!files.has_query());

        let results = files.search("find report");
        // Prefix match first, then the word prefix, then the substring.
        assert_eq!(
            names(&results),
            ["report.pdf", "Quarterly Report.key", "oldreport.txt"]
        );
        assert_eq!(results[0].description, "~/Documents");
        assert_eq!(
            results[0].icon,
            Some(IconSource::File(PathBuf::from(
                "/Users/me/Documents/report.pdf"
            )))
        );
        assert_eq!(
            files.activate(1),
            Some(Action::OpenFile(PathBuf::from(
                "/Users/me/Desktop/Quarterly Report.key"
            )))
        );
        assert_eq!(files.activate(9), None);
    }

    #[test]
    fn a_new_name_needs_a_new_search_and_stale_results_are_kept_apart() {
        let (mut files, _) = plugin(&["/Users/me/a.txt"]);
        files.search("find a");
        let first = files.take_query().unwrap();
        // The user typed on before the first search finished.
        files.search("find ab");
        let second = files.take_query().expect("the new name is searched too");
        let (name, result) = first.run();
        files.set_results(name, result);
        // Results for "a" do not answer "ab".
        assert_eq!(names(&files.search("find ab")), ["Searching for “ab”…"]);
        let (name, result) = second.run();
        files.set_results(name, Ok(result.unwrap().into_iter().take(0).collect()));
        assert_eq!(
            names(&files.search("find ab")),
            ["No files named like “ab”"]
        );
    }

    #[test]
    fn errors_and_the_bare_prefix_show_hints_without_actions() {
        let (mut files, _) = plugin(&[]);
        assert_eq!(names(&files.search("find ")), ["Find files"]);
        assert!(!files.has_query());
        files.search("find x");
        let query = files.take_query().unwrap();
        files.set_results(query.name, Err("Spotlight did not answer in time".into()));
        let results = files.search("find x");
        assert_eq!(names(&results), ["File search failed"]);
        assert_eq!(results[0].description, "Spotlight did not answer in time");
        assert_eq!(files.activate(0), None);
        assert!(files.search("report").is_empty());
        files.clear();
        assert_eq!(names(&files.search("find x")), ["Searching for “x”…"]);
    }

    /// The real runner against a stand-in `mdfind` script: argument
    /// passing, NUL-separated output, the read limit and the timeout.
    #[test]
    fn mdfind_runner_reads_nul_separated_paths_and_stops_early() {
        let tmp = TempDir::new("mdfind");
        let args_file = tmp.path().join("args");
        let script = tmp.path().join("mdfind");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '/a/one\\000/b/two words\\000/c/three\\000'\nexec sleep 30\n",
                args_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let runner = Mdfind {
            program: script,
            timeout: Duration::from_secs(10),
        };
        let started = std::time::Instant::now();
        let paths = runner
            .search("two words", Some(Path::new("/Users/me")), 2)
            .unwrap();
        assert_eq!(
            paths,
            [PathBuf::from("/a/one"), PathBuf::from("/b/two words")]
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stopped at the limit"
        );
        assert_eq!(
            std::fs::read_to_string(&args_file).unwrap(),
            "-0\n-onlyin\n/Users/me\n-name\ntwo words\n"
        );

        // Fewer results than the limit: the partial list after the timeout.
        let runner = Mdfind {
            timeout: Duration::from_millis(300),
            ..runner
        };
        let paths = runner.search("x", None, 10).unwrap();
        assert_eq!(paths.len(), 3);

        let missing = Mdfind {
            program: tmp.path().join("missing"),
            timeout: Duration::from_millis(100),
        };
        assert!(
            missing
                .search("x", None, 1)
                .unwrap_err()
                .contains("cannot run")
        );
    }

    #[test]
    fn mdfind_runner_reports_a_search_that_never_answers() {
        let tmp = TempDir::new("mdfind-slow");
        let script = tmp.path().join("mdfind");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let runner = Mdfind {
            program: script,
            timeout: Duration::from_millis(200),
        };
        assert_eq!(
            runner.search("x", None, 5),
            Err("Spotlight did not answer in time".to_owned())
        );
    }
}
