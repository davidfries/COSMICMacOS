// SPDX-License-Identifier: GPL-3.0-or-later
//! Crash-safe frame journal.
//!
//! Before the daemon moves a window for the first time (park or tile) it writes
//! the window's **original** frame to
//! `~/Library/Application Support/cosmic/frame-journal.ron`.  If the process
//! crashes, the next start replays the journal and restores every window.  A
//! clean shutdown restores the same frames and deletes the file.
//!
//! Writes are atomic (write to `.tmp`, then rename) so a crash in the middle of
//! a journal update cannot corrupt the previous copy.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use cosmic_mac_protocol::{Rect, WindowId};
use serde::{Deserialize, Serialize};

use crate::backend::PlatformBackend;

/// One saved frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameEntry {
    /// Window id the frame belongs to.
    pub wid: WindowId,
    /// Frame before the daemon touched the window.
    pub frame: Rect,
}

/// In-memory journal plus its on-disk path.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    entries: BTreeMap<WindowId, Rect>,
}

impl Journal {
    /// Create a journal backed by `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Journal {
            path: path.into(),
            entries: BTreeMap::new(),
        }
    }

    /// Journal path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of recorded windows.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `wid` already has a saved frame.
    pub fn contains(&self, wid: WindowId) -> bool {
        self.entries.contains_key(&wid)
    }

    /// Saved frame for `wid`.
    pub fn get(&self, wid: WindowId) -> Option<Rect> {
        self.entries.get(&wid).copied()
    }

    /// Every entry, sorted by window id.
    pub fn entries(&self) -> impl Iterator<Item = (WindowId, Rect)> + '_ {
        self.entries.iter().map(|(&wid, &frame)| (wid, frame))
    }

    /// Record the original frame for `wid` and persist the journal **before**
    /// returning.  Subsequent calls for the same window keep the first frame.
    pub fn record(&mut self, wid: WindowId, frame: Rect) -> bool {
        if self.entries.contains_key(&wid) {
            return false;
        }
        self.entries.insert(wid, frame);
        if let Err(error) = self.save() {
            log::error!(
                "cannot write frame journal {}: {error}",
                self.path.display()
            );
        }
        true
    }

    /// Drop one window (closed while parked) and persist.
    pub fn remove(&mut self, wid: WindowId) -> bool {
        if self.entries.remove(&wid).is_none() {
            return false;
        }
        if let Err(error) = self.save() {
            log::error!(
                "cannot write frame journal {}: {error}",
                self.path.display()
            );
        }
        true
    }

    /// Persist the journal atomically.
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let entries: Vec<FrameEntry> = self
            .entries
            .iter()
            .map(|(&wid, &frame)| FrameEntry { wid, frame })
            .collect();
        let text = ron::ser::to_string_pretty(&entries, ron::ser::PrettyConfig::default())
            .map_err(std::io::Error::other)?;
        let temporary = self.path.with_extension("ron.tmp");
        {
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &self.path)
    }

    /// Load a journal file.  A missing file yields an empty journal.
    pub fn load(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut journal = Journal::new(path.clone());
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let entries: Vec<FrameEntry> = ron::from_str(&text)
                    .map_err(|error| std::io::Error::other(format!("{error}")))?;
                for entry in entries {
                    journal.entries.entry(entry.wid).or_insert(entry.frame);
                }
                Ok(journal)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(journal),
            Err(error) => Err(error),
        }
    }

    /// Restore every recorded frame and delete the journal.
    ///
    /// Windows that no longer exist are skipped.  Returns the number restored
    /// and the per-window errors.
    pub fn restore(&mut self, backend: &dyn PlatformBackend) -> (usize, Vec<(WindowId, String)>) {
        let mut restored = 0;
        let mut errors = Vec::new();
        for (&wid, &frame) in &self.entries {
            if backend.frame(wid).is_none() {
                log::debug!("journal: window {wid} is gone, skipping");
                continue;
            }
            match backend.set_frame(wid, frame) {
                Ok(applied) => {
                    if applied.clamped {
                        log::debug!(
                            "journal: window {wid} restored to {:?} instead of {frame:?}",
                            applied.applied
                        );
                    }
                    restored += 1;
                }
                Err(error) => errors.push((wid, error)),
            }
        }
        self.entries.clear();
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("cannot remove {}: {error}", self.path.display());
        }
        (restored, errors)
    }
}

/// Replay a journal left behind by a crashed run and delete it.
///
/// Returns the number of restored windows.  A corrupt journal is renamed to
/// `.corrupt` so it is not replayed forever.
pub fn restore_stale(path: &Path, backend: &dyn PlatformBackend) -> usize {
    if !path.exists() {
        return 0;
    }
    let mut journal = match Journal::load(path) {
        Ok(journal) => journal,
        Err(error) => {
            log::error!("cannot read {}: {error}; renaming it", path.display());
            let corrupt = path.with_extension("ron.corrupt");
            let _ = std::fs::rename(path, corrupt);
            return 0;
        }
    };
    if journal.is_empty() {
        let _ = std::fs::remove_file(path);
        return 0;
    }
    let count = journal.len();
    log::warn!("found a frame journal from a previous run ({count} windows); restoring frames");
    let (restored, errors) = journal.restore(backend);
    for (wid, error) in errors {
        log::warn!("journal: cannot restore window {wid}: {error}");
    }
    restored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_only_once_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame-journal.ron");
        let mut journal = Journal::new(&path);
        let first = Rect::new(1.0, 2.0, 300.0, 200.0);
        assert!(journal.record(7, first));
        assert!(!journal.record(7, Rect::new(9.0, 9.0, 9.0, 9.0)));
        assert!(path.exists());
        let loaded = Journal::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get(7), Some(first));
    }

    #[test]
    fn remove_prunes_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame-journal.ron");
        let mut journal = Journal::new(&path);
        journal.record(1, Rect::new(0.0, 0.0, 10.0, 10.0));
        journal.record(2, Rect::new(0.0, 0.0, 20.0, 20.0));
        assert!(journal.remove(1));
        assert!(!journal.remove(1));
        let loaded = Journal::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains(2));
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::load(dir.path().join("none.ron")).unwrap();
        assert!(journal.is_empty());
    }

    #[test]
    fn corrupt_file_is_renamed_by_restore_stale() {
        struct Noop;
        impl PlatformBackend for Noop {
            fn set_frame(
                &self,
                _: WindowId,
                rect: Rect,
            ) -> Result<crate::backend::AppliedFrame, String> {
                Ok(crate::backend::AppliedFrame::new(rect, rect))
            }
            fn size_constraints(
                &self,
                _: WindowId,
            ) -> Result<Option<crate::backend::SizeConstraints>, String> {
                Ok(None)
            }
            fn raise(&self, _: WindowId) -> Result<(), String> {
                Ok(())
            }
            fn focus(&self, _: WindowId) -> Result<(), String> {
                Ok(())
            }
            fn close(&self, _: WindowId) -> Result<(), String> {
                Ok(())
            }
            fn park(&self, _: WindowId, _: Rect) -> Result<Option<Rect>, String> {
                Ok(None)
            }
            fn unpark(&self, _: WindowId) -> Result<Option<Rect>, String> {
                Ok(None)
            }
            fn saved_frame(&self, _: WindowId) -> Option<Rect> {
                None
            }
            fn parked(&self) -> Vec<(WindowId, Rect)> {
                Vec::new()
            }
            fn forget(&self, _: WindowId) {}
            fn restore_all(&self) -> Vec<(WindowId, String)> {
                Vec::new()
            }
            fn frame(&self, _: WindowId) -> Option<Rect> {
                None
            }
            fn window_info(&self, _: WindowId) -> Option<cosmic_mac_protocol::ToplevelInfo> {
                None
            }
            fn windows(&self) -> Vec<cosmic_mac_protocol::ToplevelInfo> {
                Vec::new()
            }
            fn set_stack_bar(
                &self,
                _: Option<cosmic_mac_protocol::StackInfo>,
            ) -> Result<(), String> {
                Ok(())
            }
            fn main_work_area(&self) -> Rect {
                Rect::ZERO
            }
            fn display_frame(&self, _: WindowId) -> Rect {
                Rect::ZERO
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame-journal.ron");
        std::fs::write(&path, "not ron").unwrap();
        assert_eq!(restore_stale(&path, &Noop), 0);
        assert!(!path.exists());
        assert!(dir.path().join("frame-journal.ron.corrupt").exists());
    }
}
