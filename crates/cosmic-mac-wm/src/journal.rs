// SPDX-License-Identifier: GPL-3.0-or-later
//! Crash-safe frame journal.
//!
//! Before the daemon moves a window for the first time (park or tile) it writes
//! the window's **original** frame to
//! `~/Library/Application Support/cosmic/frame-journal.ron`.  If the process
//! crashes, the next start replays the journal and restores every window.  A
//! clean shutdown restores the same frames and deletes the file.
//!
//! # Failed restores
//!
//! A restore can fail for a window that still exists, for example with an AX
//! error while its app is still busy right after a `kill -9`.  Such an entry
//! is never dropped, because the window may still be parked at the screen
//! corner and the entry is the only record of where it belongs.
//! [`Journal::restore`] drops restored entries and entries of windows that no
//! longer exist, and writes the failed ones back.  The policy:
//!
//! 1. At startup, [`restore_stale`] makes up to three passes under
//!    [`RetryPolicy::STARTUP`] (200 ms apart, no retry starting later than
//!    1 s after the first pass).  Each retry only replays the entries the
//!    previous pass could not restore.
//! 2. Entries that still fail stay in the file.  The daemon opens its journal
//!    from that file ([`Journal::open`]), so a surviving entry stays the
//!    window's original frame and is preferred over the window's current,
//!    possibly parked, frame.
//! 3. A clean shutdown (and `Request::Restore`) retries them together with
//!    every other entry; whatever still fails is kept for the next start.
//!
//! # Window id reuse
//!
//! Kept entries can outlive their window, and window ids restart low after a
//! reboot, so an id in the journal may name another app's window by now.
//! Every entry records the owning pid.  An entry whose window now belongs to
//! a different process is treated like a vanished window and dropped, both
//! on restore and when the daemon first touches the window.  Entries written
//! before the pid was recorded (or whose owner was unknown) are accepted.
//!
//! Writes are atomic (write to `.tmp`, then rename) so a crash in the middle of
//! a journal update cannot corrupt the previous copy.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cosmic_mac_protocol::{Rect, WindowId};
use serde::{Deserialize, Serialize};

use crate::backend::{PlatformBackend, is_parked_frame};

/// One saved frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameEntry {
    /// Window id the frame belongs to.
    pub wid: WindowId,
    /// Frame before the daemon touched the window.
    pub frame: Rect,
    /// Process that owned the window when the frame was recorded.  `None` in
    /// journals written before the field existed, or when the owner was
    /// unknown; such entries are accepted for any owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
}

/// One entry in memory.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Saved {
    frame: Rect,
    pid: Option<i32>,
    /// Loaded from disk (left by an earlier run) and not yet adopted by this
    /// one; see [`Journal::is_inherited`].
    inherited: bool,
}

/// In-memory journal plus its on-disk path.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    entries: BTreeMap<WindowId, Saved>,
}

impl Journal {
    /// Create a journal backed by `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Journal {
            path: path.into(),
            entries: BTreeMap::new(),
        }
    }

    /// Open the journal at `path` together with the entries already on disk.
    ///
    /// The daemon starts from the file [`restore_stale`] leaves behind, which
    /// only holds entries whose replay failed; those stay the windows'
    /// original frames.  A missing file yields an empty journal, an unreadable
    /// one is logged and treated as empty.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        match Journal::load(path.clone()) {
            Ok(journal) => {
                if !journal.is_empty() {
                    log::warn!(
                        "frame journal {} still holds {} windows a previous run could not \
                         restore; keeping their saved frames",
                        path.display(),
                        journal.len()
                    );
                }
                journal
            }
            Err(error) => {
                log::error!(
                    "cannot read {}: {error}; starting with an empty journal",
                    path.display()
                );
                Journal::new(path)
            }
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
        self.entries.get(&wid).map(|saved| saved.frame)
    }

    /// Owner pid recorded with `wid`'s frame, when known.
    pub fn pid(&self, wid: WindowId) -> Option<i32> {
        self.entries.get(&wid).and_then(|saved| saved.pid)
    }

    /// Whether `wid`'s entry was left by an earlier run and this run has not
    /// [adopted](Journal::adopt) it yet.
    pub fn is_inherited(&self, wid: WindowId) -> bool {
        self.entries.get(&wid).is_some_and(|saved| saved.inherited)
    }

    /// Accept an inherited entry as this run's record for `wid`, once its
    /// owner has been checked.
    pub fn adopt(&mut self, wid: WindowId) {
        if let Some(saved) = self.entries.get_mut(&wid) {
            saved.inherited = false;
        }
    }

    /// `(recorded, current)` owner pids when `wid`'s entry names an owner and
    /// the window now belongs to a different process: the id was reused and
    /// the entry is not this window's.  Unknown pids on either side are
    /// accepted, and the backend is only asked when the entry has a pid.
    pub fn owner_changed(
        &self,
        wid: WindowId,
        backend: &dyn PlatformBackend,
    ) -> Option<(i32, i32)> {
        self.pid_changed(wid, owner_pid(backend, wid))
    }

    /// [`Journal::owner_changed`] for an already-read `current` owner.
    fn pid_changed(&self, wid: WindowId, current: Option<i32>) -> Option<(i32, i32)> {
        let recorded = self.pid(wid)?;
        let current = current?;
        (recorded != current).then_some((recorded, current))
    }

    /// Every entry, sorted by window id.
    pub fn entries(&self) -> impl Iterator<Item = (WindowId, Rect)> + '_ {
        self.entries.iter().map(|(&wid, saved)| (wid, saved.frame))
    }

    /// Record the original frame for `wid`, owned by `pid`, and persist the
    /// journal **before** returning.  Subsequent calls for the same window
    /// keep the first frame.  A non-positive pid is stored as unknown.
    pub fn record(&mut self, wid: WindowId, frame: Rect, pid: Option<i32>) -> bool {
        if self.entries.contains_key(&wid) {
            return false;
        }
        self.entries.insert(
            wid,
            Saved {
                frame,
                pid: pid.filter(|&pid| pid > 0),
                inherited: false,
            },
        );
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
            .map(|(&wid, saved)| FrameEntry {
                wid,
                frame: saved.frame,
                pid: saved.pid,
            })
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
                    journal.entries.entry(entry.wid).or_insert(Saved {
                        frame: entry.frame,
                        pid: entry.pid.filter(|&pid| pid > 0),
                        inherited: true,
                    });
                }
                Ok(journal)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(journal),
            Err(error) => Err(error),
        }
    }

    /// Restore every recorded frame.
    ///
    /// Restored entries and entries of windows that no longer exist are
    /// dropped; a window that now belongs to another process (a reused id,
    /// see [`Journal::owner_changed`]) counts as vanished.  An entry whose
    /// restore failed is **kept** and written back
    /// atomically, so a later call (or the next start) can retry it; the file
    /// is deleted once nothing is left.  A restore the platform reports as
    /// done while the window is still at the park corner counts as failed.
    ///
    /// Returns the number restored and the errors of the kept entries.
    pub fn restore(&mut self, backend: &dyn PlatformBackend) -> (usize, Vec<(WindowId, String)>) {
        let mut restored = 0;
        let mut errors = Vec::new();
        let entries: Vec<(WindowId, Rect)> = self.entries().collect();
        for (wid, frame) in entries {
            // One read per window: each is a full Accessibility read that can
            // cost the AX timeout against a hung app.
            let info = backend.window_info(wid);
            if info.as_ref().and_then(|info| info.initial_frame).is_none() {
                log::info!("journal: window {wid} no longer exists; dropping its saved frame");
                self.entries.remove(&wid);
                continue;
            }
            let current = info.map(|info| info.pid).filter(|&pid| pid > 0);
            if let Some((recorded, current)) = self.pid_changed(wid, current) {
                log::info!(
                    "journal: window {wid} now belongs to pid {current}, not {recorded}; \
                     dropping its saved frame"
                );
                self.entries.remove(&wid);
                continue;
            }
            match backend.set_frame(wid, frame) {
                Ok(applied)
                    if applied.clamped && stayed_parked(backend, wid, frame, applied.applied) =>
                {
                    errors.push((
                        wid,
                        format!("the window stayed at the park corner {:?}", applied.applied),
                    ));
                }
                Ok(applied) => {
                    if applied.clamped {
                        log::debug!(
                            "journal: window {wid} restored to {:?} instead of {frame:?}",
                            applied.applied
                        );
                    }
                    self.entries.remove(&wid);
                    restored += 1;
                }
                Err(error) => errors.push((wid, error)),
            }
        }
        self.persist();
        (restored, errors)
    }

    /// Write the remaining entries back, or delete the file when none are
    /// left.
    fn persist(&self) {
        if self.entries.is_empty() {
            if let Err(error) = std::fs::remove_file(&self.path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("cannot remove {}: {error}", self.path.display());
            }
        } else if let Err(error) = self.save() {
            log::error!(
                "cannot write frame journal {}: {error}",
                self.path.display()
            );
        }
    }
}

/// The process that owns `wid` now, when the backend knows it (a pid of 0 is
/// the "unknown" default of [`cosmic_mac_protocol::ToplevelInfo`]).
pub fn owner_pid(backend: &dyn PlatformBackend, wid: WindowId) -> Option<i32> {
    backend
        .window_info(wid)
        .map(|info| info.pid)
        .filter(|&pid| pid > 0)
}

/// Whether a restore that `applied` instead of `requested` left the window at
/// the park corner it was supposed to leave.
fn stayed_parked(
    backend: &dyn PlatformBackend,
    wid: WindowId,
    requested: Rect,
    applied: Rect,
) -> bool {
    let display = backend.display_frame(wid);
    is_parked_frame(applied, display) && !is_parked_frame(requested, display)
}

/// How [`restore_stale`] retries entries whose restore failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Restore passes in total, the first one included.
    pub attempts: u32,
    /// Pause before each retry pass.
    pub delay: Duration,
    /// No retry pass starts later than this after the first pass started.
    pub budget: Duration,
}

impl RetryPolicy {
    /// The startup policy: 3 passes, 200 ms apart, none starting later than
    /// 1 s after the first.
    ///
    /// A transient AX error (an app still busy after the crash) usually clears
    /// within a few hundred milliseconds.  The pauses add at most 400 ms to
    /// startup.  The budget stops a hung app from multiplying the stall: each
    /// of its AX calls already waits out the 1 s messaging timeout, so a pass
    /// that slow is not repeated.  What still fails is kept for shutdown.
    pub const STARTUP: RetryPolicy = RetryPolicy {
        attempts: 3,
        delay: Duration::from_millis(200),
        budget: Duration::from_secs(1),
    };
}

/// Replay a journal left behind by a crashed run.
///
/// Returns the number of restored windows.  Failed entries are retried under
/// [`RetryPolicy::STARTUP`]; those that still fail are written back for the
/// daemon (see the module documentation), and the file is deleted once every
/// entry is restored or its window is gone.  A corrupt journal is renamed to
/// `.corrupt` so it is not replayed forever.
pub fn restore_stale(path: &Path, backend: &dyn PlatformBackend) -> usize {
    restore_stale_with(path, backend, RetryPolicy::STARTUP)
}

/// [`restore_stale`] with an explicit retry policy.
pub fn restore_stale_with(
    path: &Path,
    backend: &dyn PlatformBackend,
    policy: RetryPolicy,
) -> usize {
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
    let started = Instant::now();
    let mut restored = 0;
    let mut pass = 1;
    loop {
        let (done, errors) = journal.restore(backend);
        restored += done;
        if errors.is_empty() {
            break;
        }
        let retry = pass < policy.attempts && started.elapsed() + policy.delay < policy.budget;
        for (wid, error) in &errors {
            if retry {
                log::debug!(
                    "journal: restoring window {wid} failed (pass {pass}/{}): {error}; retrying",
                    policy.attempts
                );
            } else {
                log::warn!(
                    "journal: cannot restore window {wid} after {pass} passes: {error}; \
                     keeping its saved frame for shutdown"
                );
            }
        }
        if !retry {
            break;
        }
        std::thread::sleep(policy.delay);
        pass += 1;
    }
    restored
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use cosmic_mac_protocol::{StackInfo, ToplevelInfo};

    use crate::backend::{AppliedFrame, SizeConstraints};

    const DISPLAY: Rect = Rect::new(0.0, 0.0, 1728.0, 1080.0);
    /// Where parking leaves an 800×600 window on [`DISPLAY`].
    const PARKED: Rect = Rect::new(1727.0, 1079.0, 800.0, 600.0);
    /// Retry at once: the tests count passes, not time.
    const NO_WAIT: RetryPolicy = RetryPolicy {
        attempts: 3,
        delay: Duration::ZERO,
        budget: Duration::from_secs(60),
    };

    /// Existing windows and their frames, with scripted restore failures.
    #[derive(Default)]
    struct FakeBackend {
        state: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        frames: HashMap<WindowId, Rect>,
        /// Upcoming `set_frame` errors per window.
        failures: HashMap<WindowId, u32>,
        /// Windows whose `set_frame` succeeds without moving them.
        stuck: HashSet<WindowId>,
        /// Owner pids that differ from [`owner`].
        owners: HashMap<WindowId, i32>,
        set_frames: Vec<WindowId>,
    }

    /// Default owner pid of a test window.
    fn owner(wid: WindowId) -> i32 {
        1000 + wid as i32
    }

    impl FakeBackend {
        /// The id `wid` now names a window of process `pid`.
        fn set_owner(&self, wid: WindowId, pid: i32) {
            self.state.lock().unwrap().owners.insert(wid, pid);
        }

        fn with_windows(frames: &[(WindowId, Rect)]) -> Self {
            let backend = FakeBackend::default();
            backend.state.lock().unwrap().frames = frames.iter().copied().collect();
            backend
        }

        fn fail(&self, wid: WindowId, times: u32) {
            self.state.lock().unwrap().failures.insert(wid, times);
        }

        fn set_stuck(&self, wid: WindowId, stuck: bool) {
            let mut state = self.state.lock().unwrap();
            if stuck {
                state.stuck.insert(wid);
            } else {
                state.stuck.remove(&wid);
            }
        }

        fn set_frame_calls(&self, wid: WindowId) -> usize {
            let state = self.state.lock().unwrap();
            state.set_frames.iter().filter(|&&w| w == wid).count()
        }
    }

    impl PlatformBackend for FakeBackend {
        fn set_frame(&self, wid: WindowId, rect: Rect) -> Result<AppliedFrame, String> {
            let mut state = self.state.lock().unwrap();
            state.set_frames.push(wid);
            if let Some(left) = state.failures.get_mut(&wid)
                && *left > 0
            {
                *left -= 1;
                return Err("AX error -25204 (cannot complete)".into());
            }
            if state.stuck.contains(&wid) {
                let current = state.frames.get(&wid).copied().unwrap_or(Rect::ZERO);
                return Ok(AppliedFrame::new(rect, current));
            }
            state.frames.insert(wid, rect);
            Ok(AppliedFrame::new(rect, rect))
        }
        fn size_constraints(&self, _: WindowId) -> Result<Option<SizeConstraints>, String> {
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
        fn frame(&self, wid: WindowId) -> Option<Rect> {
            self.state.lock().unwrap().frames.get(&wid).copied()
        }
        fn window_info(&self, wid: WindowId) -> Option<ToplevelInfo> {
            let state = self.state.lock().unwrap();
            let frame = state.frames.get(&wid).copied()?;
            let pid = state.owners.get(&wid).copied().unwrap_or(owner(wid));
            Some(
                ToplevelInfo::new(wid, "com.example.app", "title")
                    .with_pid(pid)
                    .with_initial_frame(frame),
            )
        }
        fn windows(&self) -> Vec<ToplevelInfo> {
            Vec::new()
        }
        fn set_stack_bars(&self, _: Vec<StackInfo>) -> Result<(), String> {
            Ok(())
        }
        fn main_work_area(&self) -> Rect {
            DISPLAY
        }
        fn display_frame(&self, _: WindowId) -> Rect {
            DISPLAY
        }
    }

    /// A journal a crashed run left on disk, owned by the default [`owner`]s.
    fn stale_journal(dir: &Path, entries: &[(WindowId, Rect)]) -> PathBuf {
        let path = dir.join("frame-journal.ron");
        let mut journal = Journal::new(&path);
        for &(wid, frame) in entries {
            journal.record(wid, frame, Some(owner(wid)));
        }
        path
    }

    #[test]
    fn record_only_once_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame-journal.ron");
        let mut journal = Journal::new(&path);
        let first = Rect::new(1.0, 2.0, 300.0, 200.0);
        assert!(journal.record(7, first, Some(42)));
        assert!(!journal.record(7, Rect::new(9.0, 9.0, 9.0, 9.0), Some(43)));
        assert!(!journal.is_inherited(7), "recorded by this run");
        assert!(path.exists());
        let loaded = Journal::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get(7), Some(first));
        assert_eq!(loaded.pid(7), Some(42));
        assert!(loaded.is_inherited(7), "left by an earlier run");
    }

    #[test]
    fn remove_prunes_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame-journal.ron");
        let mut journal = Journal::new(&path);
        journal.record(1, Rect::new(0.0, 0.0, 10.0, 10.0), None);
        journal.record(2, Rect::new(0.0, 0.0, 20.0, 20.0), None);
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame-journal.ron");
        std::fs::write(&path, "not ron").unwrap();
        assert_eq!(restore_stale(&path, &FakeBackend::default()), 0);
        assert!(!path.exists());
        assert!(dir.path().join("frame-journal.ron.corrupt").exists());
    }

    #[test]
    fn failed_restores_are_retried_a_bounded_number_of_times_and_written_back() {
        let dir = tempfile::tempdir().unwrap();
        let original = Rect::new(10.0, 20.0, 800.0, 600.0);
        let other = Rect::new(30.0, 40.0, 500.0, 400.0);
        let path = stale_journal(dir.path(), &[(1, original), (2, other)]);
        let backend =
            FakeBackend::with_windows(&[(1, PARKED), (2, Rect::new(0.0, 0.0, 100.0, 100.0))]);
        backend.fail(1, u32::MAX);

        assert_eq!(restore_stale_with(&path, &backend, NO_WAIT), 1);

        assert_eq!(backend.set_frame_calls(1), 3, "one pass and two retries");
        assert_eq!(
            backend.set_frame_calls(2),
            1,
            "restored entries are not retried"
        );
        assert_eq!(backend.frame(2), Some(other));
        let kept = Journal::load(&path).unwrap();
        assert_eq!(
            kept.entries().collect::<Vec<_>>(),
            vec![(1, original)],
            "only the failed entry is written back, with its original frame"
        );
    }

    #[test]
    fn a_failed_restore_succeeds_on_a_later_pass() {
        let dir = tempfile::tempdir().unwrap();
        let original = Rect::new(10.0, 20.0, 800.0, 600.0);
        let path = stale_journal(dir.path(), &[(1, original)]);
        let backend = FakeBackend::with_windows(&[(1, PARKED)]);
        backend.fail(1, 2);

        assert_eq!(restore_stale_with(&path, &backend, NO_WAIT), 1);

        assert_eq!(backend.set_frame_calls(1), 3);
        assert_eq!(backend.frame(1), Some(original));
        assert!(!path.exists(), "nothing left to keep");
    }

    #[test]
    fn no_retry_starts_once_the_budget_is_spent() {
        let dir = tempfile::tempdir().unwrap();
        let original = Rect::new(10.0, 20.0, 800.0, 600.0);
        let path = stale_journal(dir.path(), &[(1, original)]);
        let backend = FakeBackend::with_windows(&[(1, PARKED)]);
        backend.fail(1, u32::MAX);
        let spent = RetryPolicy {
            attempts: 5,
            delay: Duration::ZERO,
            budget: Duration::ZERO,
        };

        assert_eq!(restore_stale_with(&path, &backend, spent), 0);

        assert_eq!(backend.set_frame_calls(1), 1);
        assert_eq!(Journal::load(&path).unwrap().get(1), Some(original));
    }

    #[test]
    fn the_startup_policy_keeps_startup_short() {
        let policy = RetryPolicy::STARTUP;
        assert!(policy.attempts >= 2, "at least one retry");
        assert!(policy.delay * (policy.attempts - 1) < policy.budget);
        assert!(policy.budget <= Duration::from_secs(1));
    }

    #[test]
    fn entries_of_vanished_windows_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let original = Rect::new(10.0, 20.0, 800.0, 600.0);
        let path = stale_journal(
            dir.path(),
            &[(1, original), (7, Rect::new(0.0, 0.0, 300.0, 200.0))],
        );
        let backend = FakeBackend::with_windows(&[(1, PARKED)]);

        assert_eq!(restore_stale_with(&path, &backend, NO_WAIT), 1);

        assert_eq!(
            backend.set_frame_calls(7),
            0,
            "a closed window is not moved"
        );
        assert!(!path.exists(), "the closed window's entry is not kept");
    }

    #[test]
    fn a_window_left_at_the_park_corner_is_not_counted_as_restored() {
        let dir = tempfile::tempdir().unwrap();
        let original = Rect::new(10.0, 20.0, 800.0, 600.0);
        let path = stale_journal(dir.path(), &[(1, original)]);
        let backend = FakeBackend::with_windows(&[(1, PARKED)]);
        // The platform reports success, but the window did not move.
        backend.set_stuck(1, true);
        let mut journal = Journal::load(&path).unwrap();

        let (restored, errors) = journal.restore(&backend);
        assert_eq!(restored, 0);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(Journal::load(&path).unwrap().get(1), Some(original));

        // A later restore (shutdown) retries the kept entry.
        backend.set_stuck(1, false);
        let (restored, errors) = journal.restore(&backend);
        assert_eq!((restored, errors.len()), (1, 0));
        assert_eq!(backend.frame(1), Some(original));
        assert!(journal.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn open_keeps_the_entries_a_failed_replay_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let original = Rect::new(10.0, 20.0, 800.0, 600.0);
        let path = stale_journal(dir.path(), &[(1, original)]);
        let backend = FakeBackend::with_windows(&[(1, PARKED)]);
        backend.fail(1, u32::MAX);
        restore_stale_with(&path, &backend, NO_WAIT);

        let journal = Journal::open(&path);
        assert_eq!(journal.get(1), Some(original));
        assert_eq!(journal.path(), path.as_path());

        assert!(Journal::open(dir.path().join("none.ron")).is_empty());
        let corrupt = dir.path().join("corrupt.ron");
        std::fs::write(&corrupt, "not ron").unwrap();
        assert!(Journal::open(&corrupt).is_empty());
    }

    #[test]
    fn an_entry_whose_window_id_now_belongs_to_another_process_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = stale_journal(dir.path(), &[(1, Rect::new(10.0, 20.0, 800.0, 600.0))]);
        // After a reboot, id 1 names a window of an unrelated app.
        let current = Rect::new(300.0, 200.0, 640.0, 480.0);
        let backend = FakeBackend::with_windows(&[(1, current)]);
        backend.set_owner(1, 4242);

        assert_eq!(restore_stale_with(&path, &backend, NO_WAIT), 0);

        assert_eq!(
            backend.set_frame_calls(1),
            0,
            "the other app's window is not moved"
        );
        assert_eq!(backend.frame(1), Some(current));
        assert!(
            !path.exists(),
            "the entry is dropped like a vanished window's"
        );
    }

    #[test]
    fn an_entry_without_a_pid_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame-journal.ron");
        // Written before the owner pid was recorded.
        std::fs::write(
            &path,
            "[(wid: 1, frame: (x: 10.0, y: 20.0, width: 800.0, height: 600.0))]",
        )
        .unwrap();
        assert_eq!(Journal::load(&path).unwrap().pid(1), None);
        let backend = FakeBackend::with_windows(&[(1, PARKED)]);
        backend.set_owner(1, 4242);

        assert_eq!(restore_stale_with(&path, &backend, NO_WAIT), 1);

        assert_eq!(backend.frame(1), Some(Rect::new(10.0, 20.0, 800.0, 600.0)));
        assert!(!path.exists());
    }

    #[test]
    fn an_entry_with_the_matching_pid_is_restored() {
        let dir = tempfile::tempdir().unwrap();
        let original = Rect::new(10.0, 20.0, 800.0, 600.0);
        let path = stale_journal(dir.path(), &[(1, original)]);
        assert_eq!(Journal::load(&path).unwrap().pid(1), Some(owner(1)));
        let backend = FakeBackend::with_windows(&[(1, PARKED)]);

        assert_eq!(restore_stale_with(&path, &backend, NO_WAIT), 1);

        assert_eq!(backend.frame(1), Some(original));
        assert!(!path.exists());
    }
}
