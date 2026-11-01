// SPDX-License-Identifier: GPL-3.0-or-later
//! Translation of layout [`Command`]s into platform calls.
//!
//! [`CommandApplier`] is the only place that performs side effects.  It also
//! owns the feedback-loop debounce (SLS echoes every programmatic move back as
//! a `WindowMoved` notification), the frame journal and the visibility set used
//! by the event loop.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmic_mac_protocol::{Command, Rect, StackInfo, WindowId};

use crate::backend::{AppliedFrame, PlatformBackend, PlatformResult, is_parked_frame};
use crate::journal::{Journal, owner_pid};

/// How long after a programmatic move we ignore platform move notifications.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy)]
struct Programmed {
    rect: Rect,
    at: Instant,
}

/// A window that refused (part of) the frame the layout assigned it.
///
/// The daemon reacts by recording the direction of the difference and probing
/// the window; see [`crate::core::Core`].  A single clamp cannot say whether the
/// app has a minimum, a maximum or a genuinely frozen dimension, so only
/// `requested` versus `applied` is carried here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClampNotice {
    /// Window the platform clamped.
    pub wid: WindowId,
    /// Frame the layout asked for.
    pub requested: Rect,
    /// Frame the window actually reached.
    pub applied: Rect,
}

/// Applies layout commands to a backend.
pub struct CommandApplier {
    backend: Arc<dyn PlatformBackend>,
    journal: Journal,
    work_area: Rect,
    programmed: HashMap<WindowId, Programmed>,
    visible: HashSet<WindowId>,
    active_workspace: u32,
    last_stack: Option<StackInfo>,
    stack_initialised: bool,
    debounce: Duration,
    /// Clamps discovered while applying the current batch.
    clamps: Vec<ClampNotice>,
}

impl CommandApplier {
    /// Create an applier.
    pub fn new(backend: Arc<dyn PlatformBackend>, journal: Journal, work_area: Rect) -> Self {
        CommandApplier {
            backend,
            journal,
            work_area,
            programmed: HashMap::new(),
            visible: HashSet::new(),
            active_workspace: 1,
            last_stack: None,
            stack_initialised: false,
            debounce: DEFAULT_DEBOUNCE,
            clamps: Vec::new(),
        }
    }

    /// Backend access for the event loop.
    pub fn backend(&self) -> &Arc<dyn PlatformBackend> {
        &self.backend
    }

    /// Current work area.
    pub fn work_area(&self) -> Rect {
        self.work_area
    }

    /// Update the work area (display change).
    pub fn set_work_area(&mut self, work_area: Rect) {
        self.work_area = work_area;
    }

    /// 1-based active workspace as last announced by the layout.
    pub fn active_workspace(&self) -> u32 {
        self.active_workspace
    }

    /// Journal access (diagnostics and tests).
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Whether the layout considers `wid` visible.
    pub fn is_visible(&self, wid: WindowId) -> bool {
        self.visible.contains(&wid)
    }

    /// Apply a batch of commands in order, logging per-command failures.
    ///
    /// Returns the windows whose frames the platform refused to honour.  The
    /// caller (the layout owner) turns those into constraint probes and a
    /// re-tile; the applier deliberately does not own the layout.
    pub fn apply(&mut self, commands: &[Command]) -> Vec<ClampNotice> {
        self.clamps.clear();
        for command in commands {
            if let Err(error) = self.apply_one(command) {
                log::warn!("{command:?} failed: {error}");
            }
        }
        self.prune_programmed();
        std::mem::take(&mut self.clamps)
    }

    /// Apply one command.
    pub fn apply_one(&mut self, command: &Command) -> PlatformResult<()> {
        match command {
            Command::SetFrame { wid, rect } => {
                // The journal must be on disk before the first frame is
                // touched, no matter what the platform answers.
                self.record_original(*wid, Some(*rect));
                // Mark the window in flight before the first AX call: the
                // move's notifications start while `set_frame` is still
                // reading the frame back, and a failed call can still have
                // moved the window part of the way.
                self.mark_programmed(*wid, *rect);
                let outcome = self.backend.set_frame(*wid, *rect)?;
                self.visible.insert(*wid);
                // Record what the window actually reached.  Recording the
                // requested frame would make the echo suppression compare
                // against a frame the window is never at.
                self.mark_programmed(*wid, outcome.applied);
                if outcome.clamped && outcome.size_differs(AppliedFrame::TOLERANCE) {
                    log::debug!(
                        "window {wid}: platform clamped {:?} to {:?}",
                        rect,
                        outcome.applied
                    );
                    self.clamps.push(ClampNotice {
                        wid: *wid,
                        requested: *rect,
                        applied: outcome.applied,
                    });
                }
                Ok(())
            }
            Command::Raise { wid } => self.backend.raise(*wid),
            Command::Hide { wid } => self.hide(*wid),
            Command::Show { wid } => self.show(*wid),
            Command::Focus { wid } => {
                self.backend.focus(*wid)?;
                Ok(())
            }
            Command::Unfocus => Ok(()),
            Command::Close { wid } => self.backend.close(*wid),
            Command::ActivateWorkspace { index } => {
                self.active_workspace = *index;
                Ok(())
            }
            Command::StackChanged { stack } => self.sync_stack_bar(stack.clone()),
        }
    }

    fn hide(&mut self, wid: WindowId) -> PlatformResult<()> {
        // Record before the first park: the journal must be on disk before the
        // window is moved off its original frame.
        self.record_original(wid, None);
        if !self.visible.remove(&wid) && self.backend.saved_frame(wid).is_some() {
            // Already parked; nothing to do.
            return Ok(());
        }
        let display = self.backend.display_frame(wid);
        self.backend.park(wid, display)?;
        Ok(())
    }

    fn show(&mut self, wid: WindowId) -> PlatformResult<()> {
        if let Some(restored) = self.backend.unpark(wid)? {
            // Unparking moves the window back from the corner; its
            // notifications are ours, not a user drag.
            self.mark_programmed(wid, restored);
        }
        self.visible.insert(wid);
        Ok(())
    }

    /// Re-apply a layout frame after the user moved a tiled window.
    ///
    /// Returns the clamp when the app refused this frame too, so the layout
    /// owner can learn from it.
    pub fn reapply_frame(&mut self, wid: WindowId, rect: Rect) -> Option<ClampNotice> {
        // In flight before the first AX call, as in `apply_one`.
        self.mark_programmed(wid, rect);
        match self.backend.set_frame(wid, rect) {
            Ok(outcome) => {
                self.mark_programmed(wid, outcome.applied);
                (outcome.clamped && outcome.size_differs(AppliedFrame::TOLERANCE)).then_some(
                    ClampNotice {
                        wid,
                        requested: rect,
                        applied: outcome.applied,
                    },
                )
            }
            Err(error) => {
                log::warn!("snapping window {wid} back failed: {error}");
                None
            }
        }
    }

    /// Record the window's original frame in the journal unless it already
    /// has one.  Called before every `SetFrame` (with the frame the layout is
    /// about to apply as `placement`) and `Hide` (no placement).
    ///
    /// - An existing entry wins.  Besides our own earlier record, that is an
    ///   entry a crashed run left behind whose replay failed: the daemon opens
    ///   the journal [`crate::journal::restore_stale`] wrote back, and such a
    ///   window may still sit at the park corner.  An inherited entry is
    ///   checked once, on the first touch: if the window now belongs to
    ///   another process (a reused id, see [`Journal::owner_changed`]), the
    ///   entry is dropped and the window recorded afresh.
    /// - Otherwise the window's current frame is recorded with its owner pid,
    ///   unless it is at the park corner ([`is_parked_frame`]): a crashed run
    ///   parked it there (or its app reopened it there).  The corner is never
    ///   recorded, since shutdown would restore the window to it and park it
    ///   for good.  The layout's `placement` is recorded instead, so shutdown
    ///   leaves the window on screen; without one (`Hide`) nothing is
    ///   recorded and the next `SetFrame` records its tile.
    pub fn record_original(&mut self, wid: WindowId, placement: Option<Rect>) {
        if self.journal.contains(wid) {
            if !self.journal.is_inherited(wid) {
                return;
            }
            match self.journal.owner_changed(wid, self.backend.as_ref()) {
                Some((recorded, current)) => {
                    log::info!(
                        "window {wid} now belongs to pid {current}, not {recorded}; dropping its \
                         saved frame"
                    );
                    self.journal.remove(wid);
                }
                None => {
                    self.journal.adopt(wid);
                    return;
                }
            }
        }
        let Some(frame) = self.backend.frame(wid) else {
            return;
        };
        let pid = owner_pid(self.backend.as_ref(), wid);
        let display = self.backend.display_frame(wid);
        if !is_parked_frame(frame, display) {
            self.journal.record(wid, frame, pid);
            return;
        }
        match placement.filter(|placement| !is_parked_frame(*placement, display)) {
            Some(placement) => {
                log::warn!(
                    "window {wid} is at the park corner {frame:?} with no saved frame (parked by \
                     a crashed run?); journaling its layout frame {placement:?} as the original"
                );
                self.journal.record(wid, placement, pid);
            }
            None => log::warn!(
                "window {wid} is at the park corner {frame:?} with no saved frame; not \
                 journaling the corner as its original"
            ),
        }
    }

    /// Remember a frame we just programmed.
    /// The frame we last programmed for `wid`, as the application applied it
    /// (a grid-snapping app reports its snapped size here, not the request).
    pub fn programmed_frame(&self, wid: WindowId) -> Option<Rect> {
        self.programmed.get(&wid).map(|programmed| programmed.rect)
    }

    pub fn mark_programmed(&mut self, wid: WindowId, rect: Rect) {
        self.programmed.insert(
            wid,
            Programmed {
                rect,
                at: Instant::now(),
            },
        );
    }

    /// Whether a platform move notification is an echo of our own change,
    /// judged as if it had been captured now.
    ///
    /// Prefer [`CommandApplier::is_echo_at`] with the time the platform
    /// callback fired; events queue while the daemon runs a long AX batch.
    pub fn is_echo(&mut self, wid: WindowId, frame: Rect) -> bool {
        self.is_echo_at(wid, frame, Instant::now())
    }

    /// Whether a platform move notification captured at `captured` is an echo
    /// of our own change.
    ///
    /// A notification matching the programmed frame is always swallowed; the
    /// entry is kept because SLS/AX often deliver several notifications for one
    /// move.  A different frame is swallowed when it was *captured* less than
    /// [`DEFAULT_DEBOUNCE`] after the frame was programmed, so intermediate
    /// updates during the move cannot make us fight ourselves.  There is no
    /// lower bound: a notification captured before we programmed the frame is
    /// stale as well.  The processing time does not matter, so notifications
    /// that sat in the queue during a long AX batch stay echoes.
    pub fn is_echo_at(&mut self, wid: WindowId, frame: Rect, captured: Instant) -> bool {
        let Some(programmed) = self.programmed.get(&wid).copied() else {
            return false;
        };
        // A notification that matches the frame we programmed is always an
        // echo.  Keep the entry: SLS/AX often deliver several notifications for
        // one move, and dropping it after the first would make the following
        // duplicates look like user drags.
        if programmed.rect.approx_eq(frame, 2.0) {
            return true;
        }
        if programmed
            .at
            .checked_add(self.debounce)
            .is_none_or(|deadline| captured < deadline)
        {
            return true;
        }
        // A different frame after the debounce is a genuine user move; stop
        // suppressing this window until the next programmatic change.
        self.programmed.remove(&wid);
        false
    }

    /// Forget all state for a closed window.
    pub fn forget_window(&mut self, wid: WindowId) {
        self.journal.remove(wid);
        self.backend.forget(wid);
        self.visible.remove(&wid);
        self.programmed.remove(&wid);
    }

    /// Stop driving a window that stays open but leaves the layout for a
    /// while (minimized, hidden with its application, or in native full
    /// screen).
    ///
    /// Unlike [`CommandApplier::forget_window`] the journal entry is **kept**:
    /// the daemon moved the window off its original frame, so a shutdown, an
    /// emergency restore or a crash replay must still put it back there.  A
    /// parked window is not unparked (it is invisible anyway); its park record
    /// is dropped without moving it.  Kept, the record would make the next
    /// `Show` restore a frame from a layout that no longer holds the window,
    /// and a later park would keep that stale frame as the one to return to.
    /// When the window comes back it is framed like a newly added window, and
    /// until then the journal restores it, from the park corner if need be.
    pub fn release_window(&mut self, wid: WindowId) {
        self.visible.remove(&wid);
        self.programmed.remove(&wid);
        if self.backend.saved_frame(wid).is_some() {
            self.backend.forget(wid);
        }
    }

    /// Update the tab bar, only calling the backend when something changed.
    pub fn sync_stack_bar(&mut self, stack: Option<StackInfo>) -> PlatformResult<()> {
        if self.stack_initialised && self.last_stack == stack {
            return Ok(());
        }
        self.stack_initialised = true;
        self.last_stack = stack.clone();
        self.backend.set_stack_bar(stack)
    }

    /// Force a tab-bar update (used when the active member is re-ordered).
    pub fn force_stack_bar(&mut self, stack: Option<StackInfo>) -> PlatformResult<()> {
        self.stack_initialised = true;
        self.last_stack = stack.clone();
        self.backend.set_stack_bar(stack)
    }

    /// Restore every frame the daemon ever changed.
    ///
    /// The journal file is removed once every entry is restored; entries that
    /// fail stay in it, so a later restore or the next start retries them.
    /// Returns the number of restored windows.
    pub fn restore_all(&mut self) -> usize {
        let (restored, errors) = self.journal.restore(self.backend.as_ref());
        for (wid, error) in errors {
            log::warn!("restore: window {wid}: {error}; keeping its saved frame in the journal");
        }
        for (wid, _) in self.backend.parked() {
            self.backend.forget(wid);
        }
        for (wid, error) in self.backend.restore_all() {
            log::warn!("restore: parked window {wid}: {error}");
        }
        self.visible.clear();
        self.programmed.clear();
        restored
    }

    fn prune_programmed(&mut self) {
        if self.programmed.len() < 256 {
            return;
        }
        let debounce = self.debounce;
        self.programmed
            .retain(|_, programmed| programmed.at.elapsed() < debounce * 4);
    }

    #[cfg(test)]
    pub(crate) fn set_debounce(&mut self, debounce: Duration) {
        self.debounce = debounce;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use cosmic_mac_protocol::{StackInfo, TabInfo, ToplevelInfo};

    use cosmic_mac_protocol::Size;

    use crate::backend::SizeConstraints;

    /// Where parking leaves a 300×200 window on the mock's 1920×1080 display.
    const PARKED: Rect = Rect::new(1919.0, 1079.0, 300.0, 200.0);

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        SetFrame(WindowId, Rect),
        Probe(WindowId),
        Raise(WindowId),
        Focus(WindowId),
        Close(WindowId),
        Park(WindowId, Rect),
        Unpark(WindowId),
        Forget(WindowId),
        RestoreAll,
        StackBar(Option<StackInfo>),
    }

    struct MockState {
        frames: HashMap<WindowId, Rect>,
        parked: HashMap<WindowId, Rect>,
        calls: Vec<Call>,
        work_area: Rect,
        display: Rect,
        /// Simulated minimum size per window.
        min_sizes: HashMap<WindowId, Size>,
        /// When set, `set_frame` fails (after the AX call would have started).
        fail_set_frame: bool,
        /// Owner pids that differ from [`owner`].
        owners: HashMap<WindowId, i32>,
    }

    /// Default owner pid of a test window.
    fn owner(wid: WindowId) -> i32 {
        1000 + wid as i32
    }

    struct MockBackend {
        state: Mutex<MockState>,
    }

    impl MockBackend {
        fn new(frames: &[(WindowId, Rect)], work_area: Rect) -> Self {
            MockBackend {
                state: Mutex::new(MockState {
                    frames: frames.iter().copied().collect(),
                    parked: HashMap::new(),
                    calls: Vec::new(),
                    work_area,
                    display: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                    min_sizes: HashMap::new(),
                    fail_set_frame: false,
                    owners: HashMap::new(),
                }),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.state.lock().unwrap().calls.clone()
        }

        fn frames(&self) -> HashMap<WindowId, Rect> {
            self.state.lock().unwrap().frames.clone()
        }

        fn set_min_size(&self, wid: WindowId, size: Size) {
            self.state.lock().unwrap().min_sizes.insert(wid, size);
        }
    }

    impl PlatformBackend for MockBackend {
        fn set_frame(&self, wid: WindowId, rect: Rect) -> PlatformResult<AppliedFrame> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::SetFrame(wid, rect));
            if state.fail_set_frame {
                return Err("cannot read the frame back".into());
            }
            let mut applied = rect;
            if let Some(min) = state.min_sizes.get(&wid) {
                applied.width = applied.width.max(min.width);
                applied.height = applied.height.max(min.height);
            }
            state.frames.insert(wid, applied);
            Ok(AppliedFrame::new(rect, applied))
        }

        fn size_constraints(&self, wid: WindowId) -> PlatformResult<Option<SizeConstraints>> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Probe(wid));
            Ok(state
                .min_sizes
                .get(&wid)
                .copied()
                .map(|min| SizeConstraints {
                    min: Some(min),
                    ..SizeConstraints::default()
                }))
        }
        fn raise(&self, wid: WindowId) -> PlatformResult<()> {
            self.state.lock().unwrap().calls.push(Call::Raise(wid));
            Ok(())
        }
        fn focus(&self, wid: WindowId) -> PlatformResult<()> {
            self.state.lock().unwrap().calls.push(Call::Focus(wid));
            Ok(())
        }
        fn close(&self, wid: WindowId) -> PlatformResult<()> {
            self.state.lock().unwrap().calls.push(Call::Close(wid));
            Ok(())
        }
        fn park(&self, wid: WindowId, display_frame: Rect) -> PlatformResult<Option<Rect>> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Park(wid, display_frame));
            let frame = state.frames.get(&wid).copied().unwrap_or(Rect::ZERO);
            state.parked.entry(wid).or_insert(frame);
            Ok(Some(frame))
        }
        fn unpark(&self, wid: WindowId) -> PlatformResult<Option<Rect>> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Unpark(wid));
            Ok(state.parked.remove(&wid))
        }
        fn saved_frame(&self, wid: WindowId) -> Option<Rect> {
            self.state.lock().unwrap().parked.get(&wid).copied()
        }
        fn parked(&self) -> Vec<(WindowId, Rect)> {
            self.state
                .lock()
                .unwrap()
                .parked
                .iter()
                .map(|(&wid, &frame)| (wid, frame))
                .collect()
        }
        fn forget(&self, wid: WindowId) {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Forget(wid));
            state.parked.remove(&wid);
        }
        fn restore_all(&self) -> Vec<(WindowId, String)> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::RestoreAll);
            let parked: Vec<_> = state.parked.drain().collect();
            for (wid, frame) in &parked {
                state.frames.insert(*wid, *frame);
            }
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
            let frames = self.frames();
            let mut windows: Vec<_> = frames
                .into_iter()
                .map(|(wid, frame)| {
                    ToplevelInfo::new(wid, "com.example.app", "title").with_initial_frame(frame)
                })
                .collect();
            windows.sort_by_key(|window| window.wid);
            windows
        }
        fn set_stack_bar(&self, stack: Option<StackInfo>) -> PlatformResult<()> {
            self.state.lock().unwrap().calls.push(Call::StackBar(stack));
            Ok(())
        }
        fn main_work_area(&self) -> Rect {
            self.state.lock().unwrap().work_area
        }
        fn display_frame(&self, _wid: WindowId) -> Rect {
            self.state.lock().unwrap().display
        }
    }

    fn applier(
        frames: &[(WindowId, Rect)],
    ) -> (CommandApplier, Arc<MockBackend>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(MockBackend::new(frames, Rect::new(0.0, 0.0, 1000.0, 800.0)));
        let journal = Journal::new(dir.path().join("frame-journal.ron"));
        let applier =
            CommandApplier::new(backend.clone(), journal, Rect::new(0.0, 0.0, 1000.0, 800.0));
        (applier, backend, dir)
    }

    #[test]
    fn every_command_maps_to_the_expected_platform_call() {
        let original = Rect::new(10.0, 20.0, 300.0, 200.0);
        let (mut applier, backend, _dir) = applier(&[(1, original)]);
        let rect = Rect::new(0.0, 0.0, 500.0, 400.0);
        let stack = StackInfo {
            workspace: 1,
            tabs: vec![TabInfo {
                wid: 1,
                title: "one".into(),
            }],
            active: 0,
            frame: rect,
        };
        applier.apply(&[
            Command::ActivateWorkspace { index: 2 },
            Command::Hide { wid: 1 },
            Command::Show { wid: 1 },
            Command::SetFrame { wid: 1, rect },
            Command::Focus { wid: 1 },
            Command::Raise { wid: 1 },
            Command::Close { wid: 1 },
            Command::StackChanged {
                stack: Some(stack.clone()),
            },
            Command::Unfocus,
        ]);
        assert_eq!(applier.active_workspace(), 2);
        let calls = backend.calls();
        assert_eq!(
            calls,
            vec![
                Call::Park(1, Rect::new(0.0, 0.0, 1920.0, 1080.0)),
                Call::Unpark(1),
                Call::SetFrame(1, rect),
                Call::Focus(1),
                Call::Raise(1),
                Call::Close(1),
                Call::StackBar(Some(stack)),
            ]
        );
        assert_eq!(backend.frames().get(&1), Some(&rect));
    }

    #[test]
    fn journal_is_written_before_the_first_frame_change() {
        let original = Rect::new(10.0, 20.0, 300.0, 200.0);
        let (mut applier, _backend, _dir) = applier(&[(1, original)]);
        let journal_path = applier.journal().path().to_path_buf();
        assert!(!journal_path.exists());
        let rect = Rect::new(0.0, 0.0, 500.0, 400.0);
        applier.apply(&[Command::SetFrame { wid: 1, rect }]);
        let journal = Journal::load(&journal_path).unwrap();
        assert_eq!(journal.get(1), Some(original));

        // A later change does not overwrite the original frame.
        applier.apply(&[Command::SetFrame {
            wid: 1,
            rect: Rect::new(1.0, 1.0, 100.0, 100.0),
        }]);
        assert_eq!(Journal::load(&journal_path).unwrap().get(1), Some(original));
    }

    #[test]
    fn park_records_original_and_unpark_restores_it() {
        let original = Rect::new(10.0, 20.0, 300.0, 200.0);
        let (mut applier, backend, _dir) = applier(&[(1, original)]);
        applier.apply(&[Command::Hide { wid: 1 }]);
        assert!(applier.journal().contains(1));
        assert_eq!(backend.saved_frame(1), Some(original));
        assert!(!applier.is_visible(1));

        applier.apply(&[Command::Show { wid: 1 }]);
        assert!(applier.is_visible(1));
        assert_eq!(backend.saved_frame(1), None);
    }

    #[test]
    fn releasing_a_window_keeps_its_journal_entry_and_drops_the_park_record() {
        let original = Rect::new(10.0, 20.0, 300.0, 200.0);
        let tile = Rect::new(0.0, 0.0, 500.0, 400.0);
        let (mut applier, backend, _dir) = applier(&[(1, original), (2, original)]);
        applier.apply(&[
            Command::SetFrame { wid: 1, rect: tile },
            Command::SetFrame { wid: 2, rect: tile },
            Command::Hide { wid: 2 },
        ]);
        assert!(applier.is_visible(1));
        assert_eq!(backend.saved_frame(2), Some(tile));
        let marker = backend.calls().len();

        applier.release_window(1);
        applier.release_window(2);

        // Nothing is moved: a parked window stays at the corner.
        assert_eq!(backend.calls()[marker..], [Call::Forget(2)]);
        assert!(!applier.is_visible(1));
        assert!(!applier.is_visible(2));
        assert_eq!(backend.saved_frame(2), None);
        // No longer in flight: its frames are not echoes any more.
        applier.set_debounce(Duration::ZERO);
        assert!(!applier.is_echo(1, tile));
        // The original frames survive for shutdown and crash replay.
        assert_eq!(applier.journal().get(1), Some(original));
        assert_eq!(applier.journal().get(2), Some(original));
    }

    #[test]
    fn duplicate_hide_does_not_park_twice() {
        let (mut applier, backend, _dir) = applier(&[(1, Rect::new(0.0, 0.0, 10.0, 10.0))]);
        applier.apply(&[Command::Hide { wid: 1 }]);
        applier.apply(&[Command::Hide { wid: 1 }]);
        let parks = backend
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Park(..)))
            .count();
        assert_eq!(parks, 1);
    }

    #[test]
    fn move_echoes_are_suppressed() {
        let (mut applier, _backend, _dir) = applier(&[(1, Rect::new(0.0, 0.0, 100.0, 100.0))]);
        let rect = Rect::new(10.0, 10.0, 200.0, 200.0);
        applier.apply(&[Command::SetFrame { wid: 1, rect }]);
        assert!(applier.is_echo(1, rect));
        // Duplicate notifications for the same programmed frame stay echoes.
        assert!(applier.is_echo(1, rect));
        applier.set_debounce(Duration::ZERO);
        // A different frame after the debounce is a user move.
        assert!(!applier.is_echo(1, Rect::new(0.0, 0.0, 50.0, 50.0)));
    }

    #[test]
    fn intermediate_move_echoes_are_suppressed_during_debounce() {
        let (mut applier, _backend, _dir) = applier(&[(1, Rect::new(0.0, 0.0, 100.0, 100.0))]);
        let rect = Rect::new(10.0, 10.0, 200.0, 200.0);
        applier.apply(&[Command::SetFrame { wid: 1, rect }]);
        assert!(applier.is_echo(1, Rect::new(0.0, 0.0, 50.0, 50.0)));
        // Simulate the debounce having expired.
        applier.set_debounce(Duration::ZERO);
        assert!(!applier.is_echo(1, Rect::new(0.0, 0.0, 50.0, 50.0)));
    }

    #[test]
    fn echoes_are_judged_by_capture_time_not_processing_time() {
        let (mut applier, _backend, _dir) = applier(&[(1, Rect::new(0.0, 0.0, 100.0, 100.0))]);
        let queued = Instant::now();
        let rect = Rect::new(10.0, 10.0, 200.0, 200.0);
        applier.apply(&[Command::SetFrame { wid: 1, rect }]);
        let intermediate = Rect::new(0.0, 0.0, 50.0, 50.0);
        applier.set_debounce(Duration::from_millis(500));

        // Captured inside the debounce: an echo whenever it is processed.
        assert!(applier.is_echo_at(1, intermediate, Instant::now()));
        // Captured before the frame was programmed: stale, even with no
        // debounce left at all.
        applier.set_debounce(Duration::ZERO);
        assert!(applier.is_echo_at(1, intermediate, queued));
        // Captured after the debounce: a user move, even though it is
        // processed inside it.
        applier.set_debounce(Duration::from_millis(500));
        let late = Instant::now() + Duration::from_millis(600);
        assert!(!applier.is_echo_at(1, intermediate, late));
        // The user move ended the suppression for this window.
        assert!(!applier.is_echo_at(1, rect, Instant::now()));
    }

    #[test]
    fn a_failed_set_frame_still_leaves_the_window_in_flight() {
        let (mut applier, backend, _dir) = applier(&[(1, Rect::new(0.0, 0.0, 100.0, 100.0))]);
        backend.state.lock().unwrap().fail_set_frame = true;
        let rect = Rect::new(10.0, 10.0, 200.0, 200.0);
        applier.apply(&[Command::SetFrame { wid: 1, rect }]);
        // The window may have moved part of the way before the call failed;
        // its notifications are still ours.
        applier.set_debounce(Duration::ZERO);
        assert!(applier.is_echo(1, rect));
        assert!(applier.reapply_frame(1, rect).is_none());
        assert!(applier.is_echo(1, rect));
    }

    #[test]
    fn unparking_records_the_restored_frame_as_programmed() {
        let original = Rect::new(10.0, 20.0, 300.0, 200.0);
        let (mut applier, _backend, _dir) = applier(&[(1, original)]);
        applier.apply(&[Command::Hide { wid: 1 }]);
        applier.apply(&[Command::Show { wid: 1 }]);
        applier.set_debounce(Duration::ZERO);
        assert!(
            applier.is_echo(1, original),
            "the move back from the park corner is not a user drag"
        );
    }

    #[test]
    fn stack_bar_updates_are_deduplicated_and_forceable() {
        let (mut applier, backend, _dir) = applier(&[(1, Rect::new(0.0, 0.0, 100.0, 100.0))]);
        let stack = StackInfo {
            workspace: 1,
            tabs: vec![TabInfo {
                wid: 1,
                title: "one".into(),
            }],
            active: 0,
            frame: Rect::new(0.0, 0.0, 100.0, 100.0),
        };
        applier.sync_stack_bar(Some(stack.clone())).unwrap();
        applier.sync_stack_bar(Some(stack.clone())).unwrap();
        applier.force_stack_bar(Some(stack.clone())).unwrap();
        applier.sync_stack_bar(None).unwrap();
        let bars: Vec<_> = backend
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::StackBar(_)))
            .collect();
        assert_eq!(
            bars,
            vec![
                Call::StackBar(Some(stack.clone())),
                Call::StackBar(Some(stack)),
                Call::StackBar(None),
            ]
        );
    }

    #[test]
    fn restore_all_replays_the_journal_and_removes_it() {
        let original = Rect::new(10.0, 20.0, 300.0, 200.0);
        let (mut applier, backend, _dir) = applier(&[(1, original)]);
        applier.apply(&[Command::SetFrame {
            wid: 1,
            rect: Rect::new(0.0, 0.0, 500.0, 400.0),
        }]);
        let journal_path = applier.journal().path().to_path_buf();
        assert!(journal_path.exists());
        assert_eq!(applier.restore_all(), 1);
        assert_eq!(backend.frames().get(&1), Some(&original));
        assert!(!journal_path.exists());
    }

    #[test]
    fn forgetting_a_window_prunes_the_journal() {
        let (mut applier, _backend, _dir) = applier(&[(1, Rect::new(0.0, 0.0, 10.0, 10.0))]);
        applier.apply(&[Command::SetFrame {
            wid: 1,
            rect: Rect::new(1.0, 1.0, 20.0, 20.0),
        }]);
        assert!(applier.journal().contains(1));
        applier.forget_window(1);
        assert!(!applier.journal().contains(1));
    }

    #[test]
    fn restore_all_keeps_failed_entries_for_a_later_restore() {
        let original = Rect::new(10.0, 20.0, 300.0, 200.0);
        let (mut applier, backend, _dir) = applier(&[(1, original)]);
        applier.apply(&[Command::SetFrame {
            wid: 1,
            rect: Rect::new(0.0, 0.0, 500.0, 400.0),
        }]);
        let journal_path = applier.journal().path().to_path_buf();

        backend.state.lock().unwrap().fail_set_frame = true;
        assert_eq!(applier.restore_all(), 0);
        assert_eq!(applier.journal().get(1), Some(original));
        assert_eq!(Journal::load(&journal_path).unwrap().get(1), Some(original));

        backend.state.lock().unwrap().fail_set_frame = false;
        assert_eq!(applier.restore_all(), 1);
        assert_eq!(backend.frames().get(&1), Some(&original));
        assert!(!journal_path.exists());
    }

    #[test]
    fn a_surviving_stale_entry_is_preferred_over_the_current_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame-journal.ron");
        let original = Rect::new(10.0, 20.0, 300.0, 200.0);
        let legacy = Rect::new(40.0, 50.0, 300.0, 200.0);
        // A crashed run parked windows 1 and 2 and the replay of their entries
        // failed: the entries survived and the windows are still at the
        // corner.  Window 2's entry predates the owner pid.
        let mut stale = Journal::new(&path);
        stale.record(1, original, Some(owner(1)));
        stale.record(2, legacy, None);
        let work_area = Rect::new(0.0, 0.0, 1000.0, 800.0);
        let backend = Arc::new(MockBackend::new(&[(1, PARKED), (2, PARKED)], work_area));
        let mut applier = CommandApplier::new(backend.clone(), Journal::open(&path), work_area);
        assert!(applier.journal().is_inherited(1));

        applier.apply(&[
            Command::SetFrame {
                wid: 1,
                rect: Rect::new(0.0, 0.0, 500.0, 400.0),
            },
            Command::SetFrame {
                wid: 2,
                rect: Rect::new(500.0, 0.0, 500.0, 400.0),
            },
        ]);

        assert_eq!(applier.journal().get(1), Some(original));
        assert_eq!(
            applier.journal().get(2),
            Some(legacy),
            "a missing pid is accepted"
        );
        assert!(
            !applier.journal().is_inherited(1),
            "checked once, then adopted"
        );
        assert_eq!(Journal::load(&path).unwrap().get(1), Some(original));
        // Shutdown puts the windows back where they were before the crashed run.
        assert_eq!(applier.restore_all(), 2);
        assert_eq!(backend.frames().get(&1), Some(&original));
        assert_eq!(backend.frames().get(&2), Some(&legacy));
        assert!(!path.exists());
    }

    #[test]
    fn a_stale_entry_of_another_process_is_replaced_by_the_current_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame-journal.ron");
        // Left by a run before a reboot; id 1 now names another app's window.
        Journal::new(&path).record(1, Rect::new(10.0, 20.0, 300.0, 200.0), Some(999));
        let current = Rect::new(200.0, 100.0, 640.0, 480.0);
        let work_area = Rect::new(0.0, 0.0, 1000.0, 800.0);
        let backend = Arc::new(MockBackend::new(&[(1, current)], work_area));
        let mut applier = CommandApplier::new(backend.clone(), Journal::open(&path), work_area);

        applier.apply(&[Command::SetFrame {
            wid: 1,
            rect: Rect::new(0.0, 0.0, 500.0, 400.0),
        }]);

        assert_eq!(applier.journal().get(1), Some(current));
        assert_eq!(applier.journal().pid(1), Some(owner(1)));
        let saved = Journal::load(&path).unwrap();
        assert_eq!(
            (saved.get(1), saved.pid(1)),
            (Some(current), Some(owner(1)))
        );
        assert_eq!(applier.restore_all(), 1);
        assert_eq!(backend.frames().get(&1), Some(&current));
    }

    #[test]
    fn a_window_at_the_park_corner_journals_its_layout_frame_instead() {
        let (mut applier, backend, _dir) = applier(&[(1, PARKED)]);
        let tile = Rect::new(0.0, 0.0, 500.0, 400.0);
        applier.apply(&[Command::SetFrame { wid: 1, rect: tile }]);
        assert_eq!(applier.journal().get(1), Some(tile));

        applier.apply(&[Command::SetFrame {
            wid: 1,
            rect: Rect::new(500.0, 0.0, 500.0, 400.0),
        }]);
        // Shutdown leaves the window on screen instead of parking it for good.
        assert_eq!(applier.restore_all(), 1);
        assert_eq!(backend.frames().get(&1), Some(&tile));
    }

    #[test]
    fn hiding_a_window_at_the_park_corner_does_not_journal_the_corner() {
        let (mut applier, _backend, _dir) = applier(&[(1, PARKED)]);
        applier.apply(&[Command::Hide { wid: 1 }]);
        assert!(!applier.journal().contains(1));
        // A placement at the corner is no better than the current frame.
        applier.apply(&[Command::SetFrame {
            wid: 1,
            rect: PARKED,
        }]);
        assert!(!applier.journal().contains(1));

        let tile = Rect::new(0.0, 0.0, 500.0, 400.0);
        applier.apply(&[
            Command::Show { wid: 1 },
            Command::SetFrame { wid: 1, rect: tile },
        ]);
        assert_eq!(applier.journal().get(1), Some(tile));
    }

    #[test]
    fn clamped_frame_is_reported_and_recorded_as_programmed() {
        let (mut applier, backend, _dir) = applier(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        backend.set_min_size(1, Size::new(642.0, 300.0));
        let requested = Rect::new(0.0, 0.0, 500.0, 800.0);

        let clamps = applier.apply(&[Command::SetFrame {
            wid: 1,
            rect: requested,
        }]);

        let applied = Rect::new(0.0, 0.0, 642.0, 800.0);
        assert_eq!(
            clamps,
            vec![ClampNotice {
                wid: 1,
                requested,
                applied,
            }]
        );
        assert_eq!(backend.frames().get(&1), Some(&applied));
    }

    #[test]
    fn debounce_uses_the_applied_frame_not_the_request() {
        let (mut applier, backend, _dir) = applier(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        backend.set_min_size(1, Size::new(800.0, 600.0));
        applier.apply(&[Command::SetFrame {
            wid: 1,
            rect: Rect::new(0.0, 0.0, 500.0, 400.0),
        }]);
        // Simulate the debounce window having expired: an echo of the frame the
        // window actually reached must still be recognised as ours, otherwise
        // the daemon snaps the window back on every layout pass.
        applier.set_debounce(Duration::ZERO);
        let applied = Rect::new(0.0, 0.0, 800.0, 600.0);
        assert!(
            applier.is_echo(1, applied),
            "the applied frame must be the programmed frame"
        );
        // The requested frame is not what the window is at; it is not an echo.
        assert!(!applier.is_echo(1, Rect::new(0.0, 0.0, 500.0, 400.0)));
    }

    #[test]
    fn journal_is_written_before_a_clamped_first_frame() {
        let original = Rect::new(10.0, 20.0, 300.0, 200.0);
        let (mut applier, backend, _dir) = applier(&[(1, original)]);
        backend.set_min_size(1, Size::new(600.0, 200.0));
        let journal_path = applier.journal().path().to_path_buf();
        applier.apply(&[Command::SetFrame {
            wid: 1,
            rect: Rect::new(0.0, 0.0, 500.0, 200.0),
        }]);
        assert_eq!(Journal::load(&journal_path).unwrap().get(1), Some(original));
    }

    #[test]
    fn reapplying_a_frame_reports_a_clamp() {
        let (mut applier, backend, _dir) = applier(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        backend.set_min_size(1, Size::new(700.0, 300.0));
        let tile = Rect::new(0.0, 0.0, 500.0, 800.0);
        let clamp = applier.reapply_frame(1, tile).expect("clamp reported");
        assert_eq!(clamp.applied.width, 700.0);
        assert_eq!(clamp.requested, tile);
    }
}
