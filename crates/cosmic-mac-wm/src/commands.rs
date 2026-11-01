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

use crate::backend::{AppliedFrame, PlatformBackend, PlatformResult};
use crate::journal::Journal;

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
                self.record_original(*wid);
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
        self.record_original(wid);
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

    /// Record the window's current frame in the journal unless it is already
    /// there.  Called before the first `SetFrame`/`Hide` for a window.
    pub fn record_original(&mut self, wid: WindowId) {
        if self.journal.contains(wid) {
            return;
        }
        if let Some(frame) = self.backend.frame(wid) {
            self.journal.record(wid, frame);
        }
    }

    /// Remember a frame we just programmed.
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

    /// Restore every frame the daemon ever changed and remove the journal.
    ///
    /// Returns the number of restored windows.
    pub fn restore_all(&mut self) -> usize {
        let (restored, errors) = self.journal.restore(self.backend.as_ref());
        for (wid, error) in errors {
            log::warn!("restore: window {wid}: {error}");
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
            self.frame(wid).map(|frame| {
                ToplevelInfo::new(wid, "com.example.app", "title").with_initial_frame(frame)
            })
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
