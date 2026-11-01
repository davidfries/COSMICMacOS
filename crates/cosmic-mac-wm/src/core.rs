// SPDX-License-Identifier: GPL-3.0-or-later
//! The single-threaded layout owner.
//!
//! All mutations happen in [`Core`], which owns the [`Layout`] and the
//! [`CommandApplier`].  The tokio event loop feeds it platform events, key
//! actions and IPC requests; nothing else touches the layout.  Queries are
//! answered from the same state, so the IPC view is always a consistent
//! snapshot.
//!
//! # App-clamped frames
//!
//! Native apps silently refuse or clamp Accessibility resizes.  When the
//! platform reports an [`AppliedFrame`](crate::backend::AppliedFrame) that
//! differs from the request, [`Core`] probes the window's minimum size, feeds
//! the learned constraint back into the layout with
//! [`Layout::update_window`] and re-applies the recomputed frames so the
//! siblings can absorb the difference.  A window that still refuses its tile
//! (a frozen dimension) is reported once and then left alone: the daemon never
//! loops on it.
//!
//! # Shell windows
//!
//! The cosmicMac panel is never managed.  [`Core`] tracks the frames of shell
//! windows ([`rules::is_shell_window`]) and tiles into the display's work area
//! minus the edge strips they cover ([`reserve::reserve_edges`]), like a
//! Wayland layer-shell exclusive zone.  The panel window appears at a default
//! position and moves to the top edge afterwards, so the reservation follows
//! its move/resize notifications and is released when it closes.
//!
//! # Suspended windows
//!
//! A managed window that is minimized, hidden with its application (Cmd+H) or
//! in native full screen is **suspended**: removed from the layout so its
//! siblings re-flow and focus moves on, but remembered together with its
//! learned size constraints.  When it is back it is re-added like a new
//! window, on the active workspace, with the constraints re-attached.  Most
//! platform notifications are only triggers: a window is suspended or resumed
//! on what a fresh [`PlatformBackend::window_info`] read reports, because
//! SkyLight's window-hidden notification may also fire for Space changes.  The
//! Accessibility minimize and deminimize notifications are the exception: they
//! are authoritative, because they fire as the animation starts, while the
//! `AXMinimized` attribute still reads the old value for about half a second
//! ([`MINIMIZE_SETTLE`]).  A suspended window is not in the layout, so it is
//! never framed, parked, focused or probed.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmic_mac_layout::Layout;
use cosmic_mac_platform::PlatformEvent;
use cosmic_mac_protocol::{
    Action, Command, Config, Rect, Request, Response, ToplevelInfo, WindowId,
};

use crate::backend::PlatformBackend;
use crate::commands::{ClampNotice, CommandApplier};
use crate::constraints::{self, ConstraintStats, LearnedConstraints};
use crate::journal::Journal;
use crate::reserve;
use crate::rules;
use crate::session::{SessionState, SessionStore};

/// A detected clamp may re-tile the layout once.  Anything the re-tile
/// reports is recorded but never retried, so a window with a frozen dimension
/// cannot make the daemon bounce frames forever.
const MAX_CONSTRAINT_PASSES: u8 = 1;

/// The daemon's state machine.
pub struct Core {
    layout: Layout,
    applier: CommandApplier,
    recent_focus: Option<(WindowId, Instant)>,
    /// Longer window used to absorb the burst of AppActivated/FocusChanged
    /// events that macOS emits while a programmatic focus change settles.
    focus_debounce: Duration,
    self_pid: i32,
    session: SessionStore,
    /// Size constraints learned per window this session.  Probing moves the
    /// window, so a window is probed at most once per session.
    constraints: HashMap<WindowId, LearnedConstraints>,
    constraint_stats: ConstraintStats,
    /// While `Some`, [`Core::apply`] collects commands here instead of
    /// applying them.  [`Core::startup`] uses it to sync the whole initial
    /// layout once instead of re-applying every tile per inserted window.
    batch: Option<Vec<Command>>,
    /// The display's work area (`NSScreen.visibleFrame`), before shell
    /// reservations.  The effective area handed to the layout and the applier
    /// is this minus [`Core::shell_strips`].
    base_work_area: Rect,
    /// Last known frame of every shell window (the panel).  An empty rect
    /// (frame not known yet) reserves nothing.
    shell_strips: HashMap<WindowId, Rect>,
    /// Managed windows taken out of the layout while they are minimized,
    /// hidden or in native full screen.  Their learned constraints stay in
    /// [`Core::constraints`].
    suspended: HashMap<WindowId, Suspended>,
}

/// A managed window that is out of the layout until it is back on screen.
#[derive(Debug, Clone)]
struct Suspended {
    /// The record the layout held when the window was suspended (its `pid`
    /// is what [`PlatformEvent::AppShown`] matches).
    info: ToplevelInfo,
    /// Its application is hidden.  Only [`PlatformEvent::AppShown`] for that
    /// application clears this; a window event alone does not prove that a
    /// hidden application's window is visible again.
    app_hidden: bool,
    /// When [`PlatformEvent::WindowMinimized`] suspended it.  Until
    /// [`MINIMIZE_SETTLE`] after this, a read that reports the window not
    /// minimized does not resume it: the attribute has not caught up yet.
    minimized_at: Option<Instant>,
}

/// How long after an Accessibility minimize notification a window's
/// `AXMinimized` attribute may still read false (measured at about 0.5 s in
/// the VM, for the minimize animation).  Within it, only the deminimize
/// notification resumes the window.
const MINIMIZE_SETTLE: Duration = Duration::from_millis(1500);

/// What asks [`Core::try_resume_at`] to re-check a suspended window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeTrigger {
    /// Some notification that the window may be back; a fresh read decides.
    Read,
    /// `AXWindowDeminiaturized`: the window is no longer minimized, whatever
    /// a read still reports.
    Deminimized,
}

/// What [`Core::startup`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupReport {
    /// Windows handed to the layout.
    pub added: usize,
    /// True when the previous session's tiling order was discarded because the
    /// work area changed (or because there was no previous session).
    pub tree_rebuilt: bool,
    /// Window order used for insertion, front-to-back.
    pub order: Vec<WindowId>,
}

impl Core {
    /// Create a core with the display's initial work area.  Shell windows
    /// tracked later are reserved from it.
    ///
    /// The journal is opened with whatever is already on disk: entries a
    /// crashed run left behind and `journal::restore_stale` could not restore
    /// stay those windows' original frames.
    pub fn new(
        config: Config,
        work_area: Rect,
        backend: Arc<dyn PlatformBackend>,
        journal_path: PathBuf,
        session_path: PathBuf,
    ) -> Self {
        let layout = Layout::with_work_area(config, work_area);
        let applier = CommandApplier::new(backend, Journal::open(journal_path), work_area);
        Core {
            layout,
            applier,
            recent_focus: None,
            focus_debounce: Duration::from_millis(250),
            self_pid: std::process::id() as i32,
            session: SessionStore::new(session_path),
            constraints: HashMap::new(),
            constraint_stats: ConstraintStats::default(),
            batch: None,
            base_work_area: work_area,
            shell_strips: HashMap::new(),
            suspended: HashMap::new(),
        }
    }

    /// Layout access for the IPC snapshot and tests.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Applier access for diagnostics and tests.
    pub fn applier(&self) -> &CommandApplier {
        &self.applier
    }

    /// Constraints learned for a window, if any.
    pub fn constraints(&self, wid: WindowId) -> Option<LearnedConstraints> {
        self.constraints.get(&wid).copied()
    }

    /// Clamp/probe/retile counters (diagnostics and tests).
    pub fn constraint_stats(&self) -> ConstraintStats {
        self.constraint_stats
    }

    /// Whether `wid` is suspended: minimized, hidden or in native full screen,
    /// and out of the layout until it is back.
    pub fn is_suspended(&self, wid: WindowId) -> bool {
        self.suspended.contains_key(&wid)
    }

    /// Track shell windows that were already on screen at startup (they are
    /// not in [`PlatformBackend::windows`], which lists normal-level windows
    /// only) and reserve their strips.
    ///
    /// Call before [`Core::startup`] so the initial layout tiles into the
    /// reserved area and the session records the effective work area.  The
    /// session is deliberately not written here: `startup` still has to read
    /// the previous run's tiling order.
    pub fn track_shell_windows(&mut self, windows: Vec<ToplevelInfo>) {
        for info in windows {
            if !rules::is_shell_window(&info) {
                continue;
            }
            let frame = info.initial_frame.unwrap_or(Rect::ZERO);
            log::info!(
                "shell window {} ({}) at {frame:?}; reserving its strip",
                info.wid,
                info.app_id
            );
            self.shell_strips.insert(info.wid, frame);
        }
        self.update_effective_work_area();
    }

    /// Startup enumeration.  `windows` must be front-to-back (the order of
    /// `CGWindowListCopyWindowInfo`).
    ///
    /// The tiling tree is always rebuilt from scratch; only the *order* can be
    /// reused.  If the work area changed since the last session, the recorded
    /// order is discarded (its tiles were sized for a different screen) and the
    /// fresh enumeration order is used.
    ///
    /// Windows are inserted back-to-front next to the **largest** current tile
    /// rather than next to the previously added window.  Inserting every window
    /// next to its predecessor builds a chain of halved tiles (`1/2`, `1/4`,
    /// `1/8`, ...), which is why a restart with eight windows used to park
    /// Finder and Notes in tiny corner tiles.
    ///
    /// The inserts are batched: applied one by one, every insert re-framed the
    /// tile it split and focused/raised windows, so windows moved several
    /// times, focus changed per insert, and the daemon stayed busy long
    /// enough for queued notifications to look like user drags.  The
    /// collected commands are compacted with [`compact_commands`] and applied
    /// once at the end: one `set_frame` per window and a single focus.
    pub fn startup(&mut self, windows: Vec<ToplevelInfo>) -> StartupReport {
        let work_area = self.layout.work_area();
        let previous = self.session.load();
        let previous_area = previous.as_ref().and_then(|state| state.work_area);
        let changed = previous_area.is_some_and(|area| !area.approx_eq(work_area, 0.5));
        let tree_rebuilt = changed || previous.is_none();
        if changed {
            log::warn!(
                "work area changed since the last session ({previous_area:?} -> {work_area:?}); \
                 discarding the previous tiling tree and rebuilding it from a fresh \
                 front-to-back enumeration"
            );
        } else if previous.is_some() {
            log::info!("work area unchanged ({work_area:?}); reusing the previous tiling order");
        } else {
            log::info!("no previous layout session; building the tiling tree from scratch");
        }

        let mut windows: Vec<ToplevelInfo> = windows
            .into_iter()
            .filter(|info| !rules::is_shell_window(info) && rules::is_manageable(info))
            .filter(|info| self.layout.window_info(info.wid).is_none())
            .collect();
        if let Some(previous) = &previous
            && !changed
        {
            let recorded = &previous.tiled_order;
            windows.sort_by_key(|info| {
                recorded
                    .iter()
                    .position(|wid| *wid == info.wid)
                    .unwrap_or(usize::MAX)
            });
        }

        let front = windows.first().map(|info| info.wid);
        let order: Vec<WindowId> = windows.iter().map(|info| info.wid).collect();
        let mut added = 0;
        self.batch = Some(Vec::new());
        for info in windows.into_iter().rev() {
            if let Some(reference) = self.largest_tile() {
                self.focus_window(reference);
            }
            self.add_window(info);
            added += 1;
        }
        if let Some(front) = front
            && self.layout.window_info(front).is_some()
        {
            self.focus_window(front);
        }
        // Batching off first: the sync goes through the normal path so clamps
        // are probed and re-tiled as usual.
        let batched = self.batch.take().unwrap_or_default();
        self.apply(compact_commands(batched));
        self.persist_session();
        StartupReport {
            added,
            tree_rebuilt,
            order,
        }
    }

    /// Feed one platform event, judged as if it had been captured now.
    pub fn platform_event(&mut self, event: PlatformEvent) {
        self.platform_event_at(event, Instant::now());
    }

    /// Feed one platform event captured at `captured` (when the platform
    /// callback fired, before it was queued).
    ///
    /// Echo suppression and the focus debounce compare against the capture
    /// time, so events that waited in the queue during a long AX batch are
    /// still recognised as consequences of our own changes.
    pub fn platform_event_at(&mut self, event: PlatformEvent, captured: Instant) {
        match event {
            PlatformEvent::WindowCreated { wid, pid } => self.window_created(wid, pid),
            PlatformEvent::WindowDestroyed { wid } => self.window_destroyed(wid),
            PlatformEvent::WindowMoved { wid, frame }
            | PlatformEvent::WindowResized { wid, frame } => {
                self.window_geometry(wid, frame, captured)
            }
            PlatformEvent::WindowTitleChanged { wid, title } => self.title_changed(wid, title),
            PlatformEvent::WindowOrdered { wid } => self.window_ordered(wid),
            PlatformEvent::FocusChanged { wid, pid } => {
                if let Some(wid) = wid {
                    self.external_focus(u64::from(wid), captured);
                } else if let Some(pid) = pid {
                    self.app_activated(pid, captured);
                }
            }
            PlatformEvent::AppActivated { pid } => {
                if let Some(pid) = pid {
                    self.app_activated(pid, captured);
                }
            }
            PlatformEvent::WindowHidden { wid } => self.window_hidden(wid),
            PlatformEvent::WindowShown { wid } => {
                self.try_resume_at(u64::from(wid), captured, ResumeTrigger::Read);
            }
            PlatformEvent::WindowMinimized { wid } => self.window_minimized(wid, captured),
            PlatformEvent::WindowDeminimized { wid } => {
                self.try_resume_at(u64::from(wid), captured, ResumeTrigger::Deminimized);
            }
            PlatformEvent::AppHidden { pid } => self.app_hidden(pid),
            PlatformEvent::AppShown { pid } => self.app_shown(pid),
            PlatformEvent::WindowLevelChanged { wid } => self.window_level_changed(wid),
            PlatformEvent::DisplayChanged => {}
            PlatformEvent::UnhandledSls { event, wid, len } => {
                log::debug!("unhandled SLS event {event} wid={wid:?} len={len}");
            }
        }
    }

    fn window_created(&mut self, wid: u32, pid: i32) {
        let wid = u64::from(wid);
        if self.suspended.contains_key(&wid) {
            // SkyLight reports a window that re-appears on a Space as created;
            // a suspended one may be back.  Never add it a second time.
            self.try_resume(wid);
            return;
        }
        if pid == self.self_pid
            || self.layout.window_info(wid).is_some()
            || self.shell_strips.contains_key(&wid)
        {
            return;
        }
        let Some(info) = self.applier.backend().window_info(wid) else {
            log::debug!("window {wid} disappeared before it could be inspected");
            return;
        };
        if rules::is_shell_window(&info) {
            // Tracked even without a frame: its move/resize notifications
            // fill it in.  Never managed.
            let frame = info.initial_frame.unwrap_or(Rect::ZERO);
            log::info!(
                "shell window {wid} ({}) appeared at {frame:?}; reserving its strip",
                info.app_id
            );
            self.shell_strips.insert(wid, frame);
            self.recompute_work_area();
            return;
        }
        if !rules::is_manageable(&info) {
            log::debug!("ignoring unmanaged window {wid} ({})", info.app_id);
            return;
        }
        log::info!(
            "managing new window {wid} ({} / {:?})",
            info.app_id,
            info.title
        );
        self.add_window(info);
    }

    fn window_destroyed(&mut self, wid: u32) {
        let wid = u64::from(wid);
        if self.shell_strips.remove(&wid).is_some() {
            log::info!("shell window {wid} closed; releasing its strip");
            self.recompute_work_area();
            return;
        }
        self.stop_managing(wid);
    }

    /// A managed window that leaves the normal level (a window turned into an
    /// overlay or a floating panel) is no longer managed, like a closed one.
    /// A window that returns to level 0 is not picked up again.
    fn window_level_changed(&mut self, wid: u32) {
        let wid = u64::from(wid);
        if self.layout.window_info(wid).is_none() {
            return;
        }
        let Some(info) = self.applier.backend().window_info(wid) else {
            return;
        };
        if info.level == 0 {
            return;
        }
        log::info!(
            "window {wid} ({}) moved to level {}; no longer managing it",
            info.app_id,
            info.level
        );
        if !self.applier.is_visible(wid) {
            // Parked on another workspace: bring it back from the corner
            // first, or it would stay there once we stop managing it.
            self.apply(vec![Command::Show { wid }]);
        }
        self.stop_managing(wid);
    }

    /// Remove a window from the layout (or from the suspended set) and forget
    /// the applier's and the learned-constraint state for it.
    fn stop_managing(&mut self, wid: WindowId) {
        self.suspended.remove(&wid);
        let commands = if self.layout.window_info(wid).is_some() {
            self.layout.window_removed(wid)
        } else {
            Vec::new()
        };
        self.applier.forget_window(wid);
        self.constraints.remove(&wid);
        self.apply(commands);
    }

    /// A window was hidden or minimized, according to the platform.
    ///
    /// SkyLight's window-hidden notification may also fire when a Space
    /// change orders windows out, so it is only a trigger: the window is
    /// suspended when a fresh read reports it minimized (or in full screen).
    fn window_hidden(&mut self, wid: u32) {
        let wid = u64::from(wid);
        if self.layout.window_info(wid).is_none() {
            return;
        }
        if !self.suspend_if_away(wid) {
            log::debug!("window {wid}: hidden notification, but not minimized; ignoring it");
        }
    }

    /// Accessibility reported the window minimized: suspend it without a
    /// re-read, which would still report it on screen.
    fn window_minimized(&mut self, wid: u32, captured: Instant) {
        let wid = u64::from(wid);
        if self.layout.window_info(wid).is_none() {
            return;
        }
        self.suspend(wid, "was minimized", false);
        if let Some(suspended) = self.suspended.get_mut(&wid) {
            suspended.minimized_at = Some(captured);
        }
    }

    /// Re-read a managed window and suspend it when it is minimized or in
    /// native full screen.  Returns whether it was suspended.
    fn suspend_if_away(&mut self, wid: WindowId) -> bool {
        if self.layout.window_info(wid).is_none() {
            return false;
        }
        let Some(reason) = self
            .applier
            .backend()
            .window_info(wid)
            .as_ref()
            .and_then(away_reason)
        else {
            return false;
        };
        self.suspend(wid, reason, false);
        true
    }

    /// Take a managed window out of the layout until it is back on screen.
    ///
    /// The rest of the layout re-flows and focus moves on, as for a closed
    /// window.  Unlike [`Core::stop_managing`], the learned constraints and
    /// the journal entry are kept ([`CommandApplier::release_window`]); a
    /// window parked on another workspace stays parked and is not moved.
    fn suspend(&mut self, wid: WindowId, reason: &str, app_hidden: bool) {
        let Some(info) = self.layout.window_info(wid).cloned() else {
            return;
        };
        log::info!(
            "window {wid} ({}) {reason}; suspending it until it is back",
            info.app_id
        );
        let commands = self.layout.window_removed(wid);
        self.applier.release_window(wid);
        self.suspended.insert(
            wid,
            Suspended {
                info,
                app_hidden,
                minimized_at: None,
            },
        );
        self.apply(commands);
    }

    /// Re-read a suspended window and put it back into the layout when it is
    /// manageable again (not minimized, not in full screen, normal level).
    ///
    /// It is added like a new window, on the active workspace, with its
    /// learned constraints re-attached.  Returns whether it was resumed.
    fn try_resume(&mut self, wid: WindowId) -> bool {
        self.try_resume_at(wid, Instant::now(), ResumeTrigger::Read)
    }

    /// [`Core::try_resume`] for a trigger captured at `captured`.
    ///
    /// A window minimized less than [`MINIMIZE_SETTLE`] before `captured`
    /// stays suspended on a [`ResumeTrigger::Read`]: its attribute still
    /// reads false during the minimize animation, and the animation itself
    /// sends move and focus notifications.  [`ResumeTrigger::Deminimized`]
    /// overrides the minimized flag of the read for the same reason.
    fn try_resume_at(&mut self, wid: WindowId, captured: Instant, trigger: ResumeTrigger) -> bool {
        let Some(suspended) = self.suspended.get_mut(&wid) else {
            return false;
        };
        if trigger == ResumeTrigger::Deminimized {
            suspended.minimized_at = None;
        }
        if suspended.app_hidden {
            return false;
        }
        if suspended
            .minimized_at
            .is_some_and(|at| captured < at + MINIMIZE_SETTLE)
        {
            return false;
        }
        let had_ax_data = suspended.info.role.is_some();
        let Some(mut info) = self.applier.backend().window_info(wid) else {
            // Gone; its destroy notification cleans up.
            return false;
        };
        if trigger == ResumeTrigger::Deminimized {
            info.minimized = false;
        }
        if had_ax_data && info.role.is_none() {
            // Accessibility did not answer, so the minimized and full-screen
            // flags read as false without meaning it.  Wait for the next
            // trigger instead of tiling a window that may still be away.
            log::debug!("window {wid}: unreadable through AX; keeping it suspended");
            return false;
        }
        if rules::is_shell_window(&info) || !rules::is_manageable(&info) {
            return false;
        }
        self.suspended.remove(&wid);
        let info = match self.constraints.get(&wid).copied() {
            Some(learned) => constraints::attach(info, learned),
            None => info,
        };
        log::info!("window {wid} ({}) is back; managing it again", info.app_id);
        self.add_window(info);
        true
    }

    /// An application was hidden (Cmd+H): suspend every managed window it
    /// owns, on any workspace, and re-flow once.
    fn app_hidden(&mut self, pid: i32) {
        for suspended in self.suspended.values_mut() {
            if suspended.info.pid == pid {
                // Minimized earlier: it now needs the application shown too.
                suspended.app_hidden = true;
            }
        }
        let wids: Vec<WindowId> = self
            .layout
            .windows()
            .into_iter()
            .filter(|state| state.info.pid == pid)
            .map(|state| state.info.wid)
            .collect();
        if wids.is_empty() {
            return;
        }
        self.batched(|core| {
            for wid in wids {
                core.suspend(wid, "was hidden with its application", true);
            }
        });
    }

    /// A hidden application was shown again: resume its suspended windows
    /// that are back (a minimized one stays suspended).
    fn app_shown(&mut self, pid: i32) {
        let mut wids: Vec<WindowId> = self
            .suspended
            .iter_mut()
            .filter(|(_, suspended)| suspended.info.pid == pid)
            .map(|(wid, suspended)| {
                suspended.app_hidden = false;
                *wid
            })
            .collect();
        if wids.is_empty() {
            return;
        }
        wids.sort_unstable();
        self.batched(|core| {
            for wid in wids {
                core.try_resume(wid);
            }
        });
    }

    /// Whether `frame` contains the display's whole work area (within a
    /// pixel), which a window in native full screen does.
    ///
    /// The cheap pre-check before re-reading a window for full screen: a tile
    /// never exceeds the work area, and a tile-sized frame at the tile's
    /// position is an echo, so ordinary drags and resizes never pay for the
    /// extra read.  Uses the main display's area (multi-display is not
    /// modelled yet).
    fn contains_work_area(&self, frame: Rect) -> bool {
        let area = self.base_work_area;
        frame.x <= area.x + 1.0
            && frame.y <= area.y + 1.0
            && frame.right() + 1.0 >= area.right()
            && frame.bottom() + 1.0 >= area.bottom()
    }

    /// Run `f` with the layout commands collected, then apply them once,
    /// compacted, dropping commands for windows that have left the layout in
    /// the meantime (an earlier re-flow may have framed a window suspended
    /// later in the same batch).
    fn batched(&mut self, f: impl FnOnce(&mut Self)) {
        if self.batch.is_some() {
            f(self);
            return;
        }
        self.batch = Some(Vec::new());
        f(self);
        let mut commands = compact_commands(self.batch.take().unwrap_or_default());
        commands.retain(|command| {
            command_window(command).is_none_or(|wid| self.layout.window_info(wid).is_some())
        });
        self.apply(commands);
    }

    fn window_geometry(&mut self, wid: u32, frame: cosmic_mac_platform::Rect, captured: Instant) {
        let wid = u64::from(wid);
        let frame = rect_from_platform(frame);
        if let Some(strip) = self.shell_strips.get_mut(&wid) {
            // A failed bounds read arrives as an empty frame; keep the last
            // known one.
            if frame.is_empty() || *strip == frame {
                return;
            }
            log::debug!("shell window {wid} moved to {frame:?}");
            *strip = frame;
            self.recompute_work_area();
            return;
        }
        if self.suspended.contains_key(&wid) {
            // Leaving full screen resizes the window back; it may be manageable
            // again.
            self.try_resume_at(wid, captured, ResumeTrigger::Read);
            return;
        }
        // The size the app last accepted; read it before the echo check, which
        // forgets it once it judges this a user move.
        let accepted = self.applier.programmed_frame(wid);
        if self.applier.is_echo_at(wid, frame, captured) {
            return;
        }
        let Some(state) = self
            .layout
            .windows()
            .into_iter()
            .find(|state| state.info.wid == wid)
        else {
            return;
        };
        // Entering native full screen resizes the window over the whole
        // display.  Check for it before the snap-back below would fight the
        // transition; the extra read only happens for frames that contain the
        // work area.
        if self.contains_work_area(frame) && self.suspend_if_away(wid) {
            return;
        }
        if !state.visible {
            // A parked window being moved by us (or the compositor) is not a
            // user move; never fight it.
            return;
        }
        if state.floating {
            // Floating windows may be moved and resized freely.  Adopt the
            // user's frame so a later diff (for example unparking after a
            // workspace round trip) does not snap it back to the old placement.
            self.layout.floating_moved(wid, frame);
        }
        // The user may have resized this window by dragging an edge.  Drop the
        // platform probe cache and the learned bounds so the next clamp probes
        // the application again instead of trusting a stale minimum/maximum.
        // A pure move (title-bar drag) keeps the size, and with it a good probe.
        // Compare with the size the app last accepted, not the layout tile: a
        // grid-snapping app (Terminal) always sits a few px off its tile, so a
        // plain drag would otherwise count as a resize.
        let tolerance = crate::backend::AppliedFrame::TOLERANCE;
        let known = accepted.unwrap_or(state.frame);
        if (frame.width - known.width).abs() > tolerance
            || (frame.height - known.height).abs() > tolerance
        {
            self.invalidate_constraints(wid);
        }
        if state.floating {
            return;
        }
        // A tiled window moved by the user snaps back to its layout slot.
        if let Some(rect) = self.layout.frame_of(wid) {
            if rect.approx_eq(frame, 2.0) {
                return;
            }
            log::debug!("window {wid} moved by user; snapping back to {rect:?}");
            if let Some(clamp) = self.applier.reapply_frame(wid, rect) {
                self.handle_clamp(clamp, 0);
            }
        }
    }

    fn title_changed(&mut self, wid: u32, title: Option<String>) {
        let wid = u64::from(wid);
        let Some(existing) = self.layout.window_info(wid).cloned() else {
            return;
        };
        let mut info = self.applier.backend().window_info(wid).unwrap_or(existing);
        if let Some(title) = title {
            info.title = title;
        }
        // The fresh platform record has no learned constraints; keep them so a
        // title change does not make the layout forget a window's minimum size.
        if let Some(learned) = self.constraints.get(&wid).copied() {
            info = constraints::attach(info, learned);
        }
        let commands = self.layout.update_window(rules::normalize_for_layout(info));
        self.apply(commands);
    }

    fn window_ordered(&mut self, wid: u32) {
        let wid = u64::from(wid);
        if let Some(stack) = self.layout.active_stack()
            && stack.active_tab().is_some_and(|tab| tab.wid == wid)
            && let Err(error) = self.applier.force_stack_bar(Some(stack))
        {
            log::debug!("re-ordering tab bar above {wid} failed: {error}");
        }
    }

    fn external_focus(&mut self, wid: WindowId, captured: Instant) {
        if self.suspended.contains_key(&wid) {
            // A suspended window that takes focus is probably back (a
            // full-screen exit whose resize was read too early, a deminimize
            // whose notification was missed).  Re-check it; resuming focuses it.
            self.try_resume_at(wid, captured, ResumeTrigger::Read);
            return;
        }
        if self.layout.window_info(wid).is_none() {
            return;
        }
        if self.is_recent_focus_event_at(captured) {
            return;
        }
        log::debug!("external focus -> window {wid}");
        let commands = self.layout.focus_changed(wid);
        self.apply(commands);
    }

    fn app_activated(&mut self, pid: i32, captured: Instant) {
        if pid == self.self_pid {
            return;
        }
        if self.is_recent_focus_event_at(captured) {
            return;
        }
        // `windows()` is front-to-back; the first managed window of the
        // activated application is the one the user sees.
        let candidate = self
            .applier
            .backend()
            .windows()
            .into_iter()
            .find(|info| info.pid == pid && self.layout.window_info(info.wid).is_some());
        if let Some(info) = candidate {
            log::debug!("application {pid} activated; focusing window {}", info.wid);
            let commands = self.layout.focus_changed(info.wid);
            self.apply(commands);
        }
    }

    /// Handle a keybinding action.
    pub fn action(&mut self, action: Action) {
        // Resize mode routes arrow keys to Resize, matching cosmic-comp.
        let action = if self.layout.resize_mode() {
            match action {
                Action::Focus { direction } => Action::Resize { direction },
                other => other,
            }
        } else {
            action
        };
        let commands = self.layout.action(action);
        self.apply(commands);
    }

    /// Handle one IPC request.  Mutations go through the same `Core`; queries
    /// are answered from the layout snapshot.
    pub fn request(&mut self, request: Request) -> Response {
        match request {
            Request::ListWindows => {
                let mut windows = self.layout.windows();
                for state in &mut windows {
                    state.actual_frame = self.applier.backend().frame(state.info.wid);
                }
                Response::Windows(windows)
            }
            Request::ListWorkspaces => Response::Workspaces(self.layout.workspaces()),
            Request::Tree => Response::Tree(self.layout.tree_snapshot()),
            Request::Config => Response::Config(self.layout.config().clone()),
            Request::Action(action) => {
                self.action(action);
                Response::Ok
            }
            Request::Focus { wid } => {
                self.focus_window(wid);
                Response::Ok
            }
            Request::Restore => {
                let restored = self.applier.restore_all();
                // Stop managing the windows that were just restored: an emergency restore
                // hands control back to the user, and the next platform event would
                // otherwise re-tile everything onto the old layout.  The layout's removal
                // commands are deliberately DISCARDED - the frames were just restored and
                // must not be touched again.  New windows are still managed afterwards.
                let managed: Vec<WindowId> = self
                    .layout
                    .windows()
                    .iter()
                    .map(|state| state.info.wid)
                    .collect();
                for wid in managed {
                    let _ = self.layout.window_removed(wid);
                }
                // Suspended windows were restored from the journal as well;
                // they are not managed again when they come back either.
                for wid in std::mem::take(&mut self.suspended).into_keys() {
                    self.constraints.remove(&wid);
                }
                // The discarded commands included the tab bar's removal; hide the
                // overlay explicitly so it does not float over a restored window.
                if let Err(error) = self.applier.sync_stack_bar(None) {
                    log::debug!("hiding the tab bar after restore failed: {error}");
                }
                log::info!("restored {restored} windows on request; no longer managing them");
                Response::Ok
            }
        }
    }

    /// Display work area changed.  Shell strips are reserved from the new
    /// area, every window is re-tiled onto the result by the layout, and the
    /// result is recorded for the next restart.
    pub fn set_work_area(&mut self, work_area: Rect) {
        self.base_work_area = work_area;
        self.recompute_work_area();
    }

    /// Recompute the effective work area after the display's area or a shell
    /// strip changed; re-tile and persist when it actually changed.
    fn recompute_work_area(&mut self) {
        if self.update_effective_work_area() {
            self.persist_session();
        }
    }

    /// Hand the display's work area minus the shell strips to the applier
    /// and the layout.  Returns false (and does nothing) when it is unchanged.
    fn update_effective_work_area(&mut self) -> bool {
        let strips: Vec<Rect> = self.shell_strips.values().copied().collect();
        let work_area = reserve::reserve_edges(self.base_work_area, &strips);
        if work_area == self.applier.work_area() {
            return false;
        }
        if work_area == self.base_work_area {
            log::info!("work area changed to {work_area:?}");
        } else {
            log::info!(
                "work area changed to {work_area:?} (display {:?} minus shell strips)",
                self.base_work_area
            );
        }
        self.applier.set_work_area(work_area);
        let commands = self.layout.set_work_area(work_area);
        self.apply(commands);
        true
    }

    /// Restore every touched window; the journal keeps only the entries whose restore failed.
    pub fn shutdown(&mut self) {
        self.persist_session();
        let restored = self.applier.restore_all();
        self.constraints.clear();
        self.suspended.clear();
        log::info!("shutdown: restored {restored} windows");
    }

    fn add_window(&mut self, info: ToplevelInfo) {
        let info = rules::normalize_for_layout(info);
        let commands = self.layout.window_added(info);
        self.apply(commands);
    }

    fn focus_window(&mut self, wid: WindowId) {
        let commands = self.layout.action(Action::FocusWindow { wid });
        self.apply(commands);
    }

    /// Largest visible tiled frame on the active workspace, used to keep the
    /// startup tree balanced.
    fn largest_tile(&self) -> Option<WindowId> {
        let active = self.layout.active_workspace();
        self.layout
            .windows()
            .into_iter()
            .filter(|state| state.visible && !state.floating && state.workspace == active)
            .max_by(|a, b| a.frame.area().total_cmp(&b.frame.area()))
            .map(|state| state.info.wid)
    }

    /// Tiled windows in tree order, for the next restart.
    fn tiled_order(&self) -> Vec<WindowId> {
        let mut order = self
            .layout
            .tree_snapshot()
            .map(|tree| tree.window_ids())
            .unwrap_or_default();
        for state in self.layout.windows() {
            if !state.floating && !order.contains(&state.info.wid) {
                order.push(state.info.wid);
            }
        }
        order
    }

    fn persist_session(&self) {
        let state = SessionState {
            work_area: Some(self.layout.work_area()),
            tiled_order: self.tiled_order(),
        };
        self.session.save(&state);
    }

    fn apply(&mut self, commands: Vec<Command>) {
        let focused = commands.iter().find_map(|command| match command {
            Command::Focus { wid } => Some(*wid),
            _ => None,
        });
        if let Some(wid) = focused {
            self.recent_focus = Some((wid, Instant::now()));
        }
        if let Some(batch) = &mut self.batch {
            batch.extend(commands);
            return;
        }
        if !commands.is_empty() {
            log::debug!("commands: {commands:?}");
        }
        let clamps = self.applier.apply(&commands);
        if let Some(wid) = focused {
            // The focus call can come after a long run of `set_frame` calls in
            // the same batch; restart the debounce once it has actually been
            // made so its activation burst is still covered.
            self.recent_focus = Some((wid, Instant::now()));
        }
        for clamp in clamps {
            self.handle_clamp(clamp, 0);
        }
        // StackChanged is only emitted when the stack identity changes; a work
        // area change or a re-tile can move the stack without a new command, so
        // re-sync the tab bar after every batch.  The applier de-duplicates.
        if let Err(error) = self.applier.sync_stack_bar(self.layout.active_stack()) {
            log::debug!("tab bar update failed: {error}");
        }
    }

    /// React to a frame the platform refused: learn the window's constraint,
    /// re-tile the layout with it and re-apply the recomputed frames.
    fn handle_clamp(&mut self, clamp: ClampNotice, depth: u8) {
        if self.layout.window_info(clamp.wid).is_none() {
            // Suspended or closed since the frame was applied: never probe a
            // window the layout does not hold.
            return;
        }
        // A window minimized (or gone full screen) before its notification
        // arrived refuses its tile for that reason, not because of a size
        // limit.  Suspend it instead of probing it, which would pin a bogus
        // bound.
        if self.suspend_if_away(clamp.wid) {
            return;
        }
        if depth >= MAX_CONSTRAINT_PASSES {
            // The re-tile could not fix it either (a frozen dimension).  Log
            // once per window and dimension, then leave the window alone.
            self.record_stuck(clamp);
            return;
        }
        self.learn_constraints(clamp);
        let commands = self.retile_with_constraints(clamp.wid);
        let followups = self.applier.apply(&commands);
        self.record_stuck(clamp);
        for followup in followups {
            self.handle_clamp(followup, depth + 1);
        }
    }

    /// Record one clamped frame and learn from it.
    ///
    /// A single `set_frame` result only tells us the *direction* of the
    /// refusal: a window that comes back wider than requested refused to
    /// shrink (minimum candidate), a window that comes back narrower refused
    /// to grow (maximum candidate).  It cannot tell a minimum from a genuinely
    /// frozen dimension, so the authoritative answer comes from the platform's
    /// shrink **and** grow probe.  Candidates are promoted only after that
    /// probe succeeds; a failed probe keeps them as evidence but never pins a
    /// bound.  Even then a candidate needs the probe to explain it or a repeat
    /// from a different request (see [`LearnedConstraints::promote_candidates`]),
    /// so a grid-snapping app is never pinned at a snapped size.
    fn learn_constraints(&mut self, clamp: ClampNotice) {
        let wid = clamp.wid;
        self.constraints
            .entry(wid)
            .or_default()
            .note_frame(clamp.requested, clamp.applied);

        if self
            .constraints
            .get(&wid)
            .is_some_and(|constraints| constraints.probed)
        {
            // Already probed this session: the new observation tightens the
            // confirmed bounds only when the probe explains it or it repeats an
            // earlier clamp from a different request.  A Terminal grid snap
            // does neither.
            self.constraints
                .entry(wid)
                .or_default()
                .promote_candidates();
            return;
        }

        self.constraint_stats.probes += 1;
        let probe = self.applier.backend().size_constraints(wid);
        // The probe moved the window and put it back; treat the frame it ended
        // at as programmed so the resulting notifications stay echoes.
        if let Some(frame) = self.applier.backend().frame(wid) {
            self.applier.mark_programmed(wid, frame);
        }
        let constraints = self.constraints.entry(wid).or_default();
        match probe {
            Ok(Some(learned)) => {
                log::info!("window {wid}: learned size constraints {learned:?}");
                constraints.confirm_probe(learned.min, learned.max);
            }
            Ok(None) => {
                log::debug!("window {wid}: no size constraints reported by the probe");
                constraints.fail_probe();
            }
            Err(error) => {
                // Transient (for example CGError 1000 right after a resize):
                // retry on a later clamp, but only a bounded number of times.
                let gave_up = constraints.defer_probe();
                log::debug!(
                    "window {wid}: size constraint probe failed \
                     (attempt {}/{}): {error}",
                    constraints.probe_attempts,
                    LearnedConstraints::MAX_PROBE_ATTEMPTS
                );
                if gave_up {
                    log::warn!(
                        "window {wid}: giving up on size constraint probing after {} failures",
                        constraints.probe_attempts
                    );
                }
            }
        }
    }

    /// Hand the learned constraints to the layout and return the commands that
    /// re-tile the siblings.
    fn retile_with_constraints(&mut self, wid: WindowId) -> Vec<Command> {
        let Some(info) = self.layout.window_info(wid).cloned() else {
            return Vec::new();
        };
        let learned = self.constraints.get(&wid).copied().unwrap_or_default();
        let info = constraints::attach(info, learned);
        self.constraint_stats.retiles += 1;
        let mut commands = self.layout.update_window(rules::normalize_for_layout(info));
        // Re-apply the window's own tile even when the layout only rebalanced
        // siblings: the probe moved the window a moment ago and the layout's
        // frame must win.
        if let Some(rect) = self.layout.frame_of(wid)
            && !commands
                .iter()
                .any(|command| matches!(command, Command::SetFrame { wid: w, .. } if *w == wid))
        {
            commands.push(Command::SetFrame { wid, rect });
        }
        if !commands.is_empty() {
            log::debug!("re-tiling after a clamp on window {wid}: {commands:?}");
        }
        commands
    }

    /// Compare the window's frame with its tile after a re-tile and report a
    /// dimension the app refuses to move, once.
    ///
    /// The comparison also feeds the directional candidate set, but it must
    /// **never** pin a bound by itself: `got > want` only proves the app would
    /// not shrink to `want`, not that the dimension is frozen.  If a probe has
    /// already confirmed the window, the candidate is merged into the
    /// authoritative bounds under the same rules as any other clamp.
    fn record_stuck(&mut self, clamp: ClampNotice) {
        let wid = clamp.wid;
        let Some(want) = self.layout.frame_of(wid) else {
            return;
        };
        let got = self.applier.backend().frame(wid).unwrap_or(clamp.applied);
        if got.approx_eq(want, 1.0) {
            return;
        }
        let mut details = Vec::new();
        {
            let constraints = self.constraints.entry(wid).or_default();
            constraints.note_frame(want, got);
            constraints.promote_candidates();
            if (got.width - want.width).abs() > 1.0 && !constraints.warned_width {
                constraints.warned_width = true;
                details.push(format!(
                    "width {:.0}px requested, stuck at {:.0}px",
                    want.width, got.width
                ));
            }
            if (got.height - want.height).abs() > 1.0 && !constraints.warned_height {
                constraints.warned_height = true;
                details.push(format!(
                    "height {:.0}px requested, stuck at {:.0}px",
                    want.height, got.height
                ));
            }
        }
        if details.is_empty() {
            return;
        }
        self.constraint_stats.warnings += 1;
        log::warn!(
            "window {wid}: the app does not accept its tile {want:?} ({}); \
             no sibling can absorb this right now, so it is left in place and \
             reported only once",
            details.join(", ")
        );
    }

    /// Drop learned constraints after a user-driven size change.
    ///
    /// The app's current size may now be its new natural/minimum size, and the
    /// platform probe cache is stale too, so the next clamp must re-probe.  The
    /// bounds previously attached to the layout are cleared as well: otherwise
    /// they would keep shaping the tiles, the window would never be asked for a
    /// size outside them, and no clamp (hence no re-probe) would ever happen.
    fn invalidate_constraints(&mut self, wid: WindowId) {
        self.applier.backend().forget_size_constraints(wid);
        let removed = self.constraints.remove(&wid).is_some();
        if removed {
            log::debug!("window {wid}: user resize invalidated learned size constraints");
        }
        let Some(info) = self.layout.window_info(wid).cloned() else {
            return;
        };
        if !removed && info.min_size.is_none() && info.max_size.is_none() {
            return;
        }
        let info = constraints::attach(info, LearnedConstraints::default());
        let commands = self.layout.update_window(rules::normalize_for_layout(info));
        self.apply(commands);
    }

    /// Whether a focus/activation event captured at `captured` belongs to the
    /// burst macOS emits after our own focus change.  As with the move echo,
    /// there is no lower bound: an event captured before our focus change is
    /// stale.
    fn is_recent_focus_event_at(&self, captured: Instant) -> bool {
        self.recent_focus.is_some_and(|(_, at)| {
            at.checked_add(self.focus_debounce)
                .is_none_or(|deadline| captured < deadline)
        })
    }

    #[cfg(test)]
    fn set_debounce(&mut self, debounce: Duration) {
        self.focus_debounce = debounce;
    }
}

/// Collapse a batch of layout commands to the ones whose final effect matters.
///
/// - the **last** `SetFrame` per window, at the position of that last
///   occurrence;
/// - the **last** `Focus` only; every `Raise`/`Unfocus` is dropped except the
///   last `Raise` of that finally focused window;
/// - `Hide`, `Show`, `Close` and `ActivateWorkspace` in their original order;
/// - the **last** `StackChanged` only.
fn compact_commands(commands: Vec<Command>) -> Vec<Command> {
    let final_focus = commands.iter().rev().find_map(|command| match command {
        Command::Focus { wid } => Some(*wid),
        _ => None,
    });
    let mut framed = HashSet::new();
    let mut focus_kept = false;
    let mut raise_kept = false;
    let mut stack_kept = false;
    let mut kept = Vec::with_capacity(commands.len());
    // Walk backwards so "first seen" is the last occurrence.
    for command in commands.into_iter().rev() {
        let keep = match &command {
            Command::SetFrame { wid, .. } => framed.insert(*wid),
            Command::Focus { .. } => !std::mem::replace(&mut focus_kept, true),
            Command::Raise { wid } => {
                final_focus == Some(*wid) && !std::mem::replace(&mut raise_kept, true)
            }
            Command::Unfocus => false,
            Command::StackChanged { .. } => !std::mem::replace(&mut stack_kept, true),
            Command::Hide { .. }
            | Command::Show { .. }
            | Command::Close { .. }
            | Command::ActivateWorkspace { .. } => true,
        };
        if keep {
            kept.push(command);
        }
    }
    kept.reverse();
    kept
}

/// Why a window is temporarily out of view, if it is: minimized or in native
/// full screen.  (A hidden application is reported per application, not
/// per window.)
fn away_reason(info: &ToplevelInfo) -> Option<&'static str> {
    if info.minimized {
        Some("was minimized")
    } else if info.fullscreen {
        Some("entered native full screen")
    } else {
        None
    }
}

/// The window a command targets, if any.
fn command_window(command: &Command) -> Option<WindowId> {
    match command {
        Command::SetFrame { wid, .. }
        | Command::Raise { wid }
        | Command::Hide { wid }
        | Command::Show { wid }
        | Command::Focus { wid }
        | Command::Close { wid } => Some(*wid),
        Command::Unfocus | Command::ActivateWorkspace { .. } | Command::StackChanged { .. } => None,
    }
}

fn rect_from_platform(rect: cosmic_mac_platform::Rect) -> cosmic_mac_protocol::Rect {
    cosmic_mac_protocol::Rect::new(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use cosmic_mac_platform::{PlatformEvent, Rect as PlatformRect};
    use cosmic_mac_protocol::{Config, Rect, Request, Response, Size, StackInfo, ToplevelInfo};

    use crate::backend::{AppliedFrame, PlatformResult, SizeConstraints};
    use crate::session::{SessionState, SessionStore};

    const WORK_AREA: Rect = Rect::new(0.0, 0.0, 1000.0, 800.0);

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        SetFrame(WindowId, Rect),
        Probe(WindowId),
        ForgetProbe(WindowId),
        Focus(WindowId),
        Park(WindowId, Rect),
        Unpark(WindowId),
        StackBar(Option<StackInfo>),
    }

    struct State {
        frames: HashMap<WindowId, Rect>,
        order: Vec<WindowId>,
        parked: HashMap<WindowId, Rect>,
        calls: Vec<Call>,
        work_area: Rect,
        min_sizes: HashMap<WindowId, Size>,
        frozen_width: HashMap<WindowId, f64>,
        frozen_height: HashMap<WindowId, f64>,
        /// Resize-grid cell per window (Terminal-style snapping).
        grids: HashMap<WindowId, Size>,
        /// Enforced maximum per window, and whether the probe reports it.
        max_sizes: HashMap<WindowId, (Size, bool)>,
        probe_fails: bool,
        /// Per-window metadata that differs from the defaults (app id
        /// `com.example.app`, its own pid, level 0, not minimized, not full
        /// screen, readable through AX).
        overrides: HashMap<WindowId, InfoOverride>,
        /// `window_info` calls, to check that hot paths do not re-read.
        info_reads: usize,
    }

    #[derive(Debug, Clone, Default)]
    struct InfoOverride {
        app_id: Option<&'static str>,
        pid: Option<i32>,
        level: i32,
        minimized: bool,
        fullscreen: bool,
        /// AX does not answer: like the real backend, the record falls back to
        /// CoreGraphics data, without a role and with the minimized and
        /// full-screen flags reading false.
        ax_unreadable: bool,
    }

    impl State {
        /// The record the backend reports for `wid` at `frame`.
        fn toplevel(&self, wid: WindowId, frame: Rect) -> ToplevelInfo {
            let info = self.overrides.get(&wid).cloned().unwrap_or_default();
            let mut toplevel =
                ToplevelInfo::new(wid, info.app_id.unwrap_or("com.example.app"), "title")
                    .with_pid(info.pid.unwrap_or(test_pid(wid)))
                    .with_initial_frame(frame)
                    .with_level(info.level);
            if !info.ax_unreadable {
                toplevel.role = Some("AXWindow".into());
                toplevel = toplevel
                    .with_minimized(info.minimized)
                    .with_fullscreen(info.fullscreen);
            }
            toplevel
        }

        /// Every window, front-to-back.
        fn toplevels(&self) -> Vec<ToplevelInfo> {
            self.order
                .iter()
                .filter_map(|wid| {
                    let frame = self.frames.get(wid).copied()?;
                    Some(self.toplevel(*wid, frame))
                })
                .collect()
        }
    }

    struct TestBackend {
        state: Mutex<State>,
    }

    impl TestBackend {
        fn new(frames: &[(WindowId, Rect)], work_area: Rect) -> Self {
            TestBackend {
                state: Mutex::new(State {
                    frames: frames.iter().copied().collect(),
                    order: frames.iter().map(|(wid, _)| *wid).collect(),
                    parked: HashMap::new(),
                    calls: Vec::new(),
                    work_area,
                    min_sizes: HashMap::new(),
                    frozen_width: HashMap::new(),
                    frozen_height: HashMap::new(),
                    grids: HashMap::new(),
                    max_sizes: HashMap::new(),
                    probe_fails: false,
                    overrides: HashMap::new(),
                    info_reads: 0,
                }),
            }
        }

        /// A window that appears after startup (front-most).
        fn add_window(&self, wid: WindowId, frame: Rect) {
            let mut state = self.state.lock().unwrap();
            state.frames.insert(wid, frame);
            state.order.insert(0, wid);
        }

        /// A window that closed.
        fn remove_window(&self, wid: WindowId) {
            let mut state = self.state.lock().unwrap();
            state.frames.remove(&wid);
            state.order.retain(|other| *other != wid);
        }

        fn set_app_id(&self, wid: WindowId, app_id: &'static str) {
            let mut state = self.state.lock().unwrap();
            state.overrides.entry(wid).or_default().app_id = Some(app_id);
        }

        fn set_level(&self, wid: WindowId, level: i32) {
            let mut state = self.state.lock().unwrap();
            state.overrides.entry(wid).or_default().level = level;
        }

        fn set_minimized(&self, wid: WindowId, minimized: bool) {
            let mut state = self.state.lock().unwrap();
            state.overrides.entry(wid).or_default().minimized = minimized;
        }

        fn set_fullscreen(&self, wid: WindowId, fullscreen: bool) {
            let mut state = self.state.lock().unwrap();
            state.overrides.entry(wid).or_default().fullscreen = fullscreen;
        }

        fn set_pid(&self, wid: WindowId, pid: i32) {
            let mut state = self.state.lock().unwrap();
            state.overrides.entry(wid).or_default().pid = Some(pid);
        }

        fn set_ax_unreadable(&self, wid: WindowId, unreadable: bool) {
            let mut state = self.state.lock().unwrap();
            state.overrides.entry(wid).or_default().ax_unreadable = unreadable;
        }

        /// Move or resize a window behind the daemon's back (the user, the
        /// app or the system did it).
        fn set_frame_externally(&self, wid: WindowId, frame: Rect) {
            self.state.lock().unwrap().frames.insert(wid, frame);
        }

        fn info_reads(&self) -> usize {
            self.state.lock().unwrap().info_reads
        }

        fn calls(&self) -> Vec<Call> {
            self.state.lock().unwrap().calls.clone()
        }

        fn frame(&self, wid: WindowId) -> Option<Rect> {
            self.state.lock().unwrap().frames.get(&wid).copied()
        }

        fn set_min_size(&self, wid: WindowId, size: Size) {
            self.state.lock().unwrap().min_sizes.insert(wid, size);
        }

        fn simulate_frozen_width(&self, wid: WindowId, width: f64) {
            self.state.lock().unwrap().frozen_width.insert(wid, width);
        }

        /// Terminal-style resize grid: widths round down to `cell.width`,
        /// heights to the nearest `cell.height`.  The probe reports no maximum,
        /// as the platform's grow probe does for a snapped grow.
        fn simulate_grid(&self, wid: WindowId, cell: Size) {
            self.state.lock().unwrap().grids.insert(wid, cell);
        }

        /// An enforced maximum; `probe_reports` says whether the probe finds it.
        fn set_max_size(&self, wid: WindowId, size: Size, probe_reports: bool) {
            self.state
                .lock()
                .unwrap()
                .max_sizes
                .insert(wid, (size, probe_reports));
        }

        fn fail_probes(&self) {
            self.state.lock().unwrap().probe_fails = true;
        }

        fn allow_probes(&self) {
            self.state.lock().unwrap().probe_fails = false;
        }

        fn probes(&self) -> usize {
            self.calls()
                .into_iter()
                .filter(|call| matches!(call, Call::Probe(_)))
                .count()
        }

        fn frames(&self) -> HashMap<WindowId, Rect> {
            self.state.lock().unwrap().frames.clone()
        }
    }

    impl PlatformBackend for TestBackend {
        fn set_frame(&self, wid: WindowId, rect: Rect) -> PlatformResult<AppliedFrame> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::SetFrame(wid, rect));
            let mut applied = rect;
            if let Some(frozen) = state.frozen_width.get(&wid) {
                applied.width = *frozen;
            }
            if let Some(frozen) = state.frozen_height.get(&wid) {
                applied.height = *frozen;
            }
            if let Some(cell) = state.grids.get(&wid) {
                applied.width = (applied.width / cell.width).floor() * cell.width;
                applied.height = (applied.height / cell.height).round() * cell.height;
            }
            if let Some((max, _)) = state.max_sizes.get(&wid) {
                applied.width = applied.width.min(max.width);
                applied.height = applied.height.min(max.height);
            }
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
            if state.probe_fails {
                return Err("probe unsupported".into());
            }
            let min = state.min_sizes.get(&wid).copied();
            let mut min_width = min.map(|size| size.width);
            let mut min_height = min.map(|size| size.height);
            let mut max_width = None;
            let mut max_height = None;
            if let Some((max, true)) = state.max_sizes.get(&wid).copied() {
                max_width = Some(max.width);
                max_height = Some(max.height);
            }
            if let Some(frozen) = state.frozen_width.get(&wid) {
                min_width = Some(*frozen);
                max_width = Some(*frozen);
            }
            if let Some(frozen) = state.frozen_height.get(&wid) {
                min_height = Some(*frozen);
                max_height = Some(*frozen);
            }
            let min = (min_width.is_some() || min_height.is_some())
                .then(|| Size::new(min_width.unwrap_or(0.0), min_height.unwrap_or(0.0)));
            let max = (max_width.is_some() || max_height.is_some()).then(|| {
                Size::new(
                    max_width.unwrap_or(f64::MAX),
                    max_height.unwrap_or(f64::MAX),
                )
            });
            Ok(Some(SizeConstraints { min, max }))
        }

        fn forget_size_constraints(&self, wid: WindowId) {
            self.state
                .lock()
                .unwrap()
                .calls
                .push(Call::ForgetProbe(wid));
        }

        fn raise(&self, _: WindowId) -> PlatformResult<()> {
            Ok(())
        }
        fn focus(&self, wid: WindowId) -> PlatformResult<()> {
            self.state.lock().unwrap().calls.push(Call::Focus(wid));
            Ok(())
        }
        fn close(&self, _: WindowId) -> PlatformResult<()> {
            Ok(())
        }
        fn park(&self, wid: WindowId, display: Rect) -> PlatformResult<Option<Rect>> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Park(wid, display));
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
            self.state.lock().unwrap().parked.remove(&wid);
        }
        fn restore_all(&self) -> Vec<(WindowId, String)> {
            Vec::new()
        }
        fn frame(&self, wid: WindowId) -> Option<Rect> {
            self.frame(wid)
        }
        fn window_info(&self, wid: WindowId) -> Option<ToplevelInfo> {
            let mut state = self.state.lock().unwrap();
            state.info_reads += 1;
            let frame = state.frames.get(&wid).copied()?;
            Some(state.toplevel(wid, frame))
        }
        fn windows(&self) -> Vec<ToplevelInfo> {
            // Unfiltered: the core must apply the level/minimized/shell rules
            // itself.
            self.state.lock().unwrap().toplevels()
        }
        fn shell_windows(&self) -> Vec<ToplevelInfo> {
            let state = self.state.lock().unwrap();
            state
                .toplevels()
                .into_iter()
                .filter(rules::is_shell_window)
                .collect()
        }
        fn set_stack_bar(&self, stack: Option<StackInfo>) -> PlatformResult<()> {
            self.state.lock().unwrap().calls.push(Call::StackBar(stack));
            Ok(())
        }
        fn main_work_area(&self) -> Rect {
            self.state.lock().unwrap().work_area
        }
        fn display_frame(&self, _: WindowId) -> Rect {
            Rect::new(0.0, 0.0, 1920.0, 1080.0)
        }
    }

    /// Every test window belongs to its own application.
    fn test_pid(wid: WindowId) -> i32 {
        1000 + wid as i32
    }

    fn core_with(
        frames: &[(WindowId, Rect)],
        work_area: Rect,
        session: Option<SessionState>,
    ) -> (Core, Arc<TestBackend>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("wm-session.ron");
        if let Some(state) = session {
            SessionStore::new(&session_path).save(&state);
        }
        let backend = Arc::new(TestBackend::new(frames, work_area));
        let core = Core::new(
            Config::default(),
            work_area,
            backend.clone(),
            dir.path().join("frame-journal.ron"),
            session_path,
        );
        (core, backend, dir)
    }

    fn core(frames: &[(WindowId, Rect)]) -> (Core, Arc<TestBackend>, tempfile::TempDir) {
        core_with(frames, WORK_AREA, None)
    }

    fn window(wid: WindowId) -> ToplevelInfo {
        ToplevelInfo::new(wid, "com.example.app", "title")
            .with_initial_frame(Rect::new(0.0, 0.0, 500.0, 400.0))
    }

    fn set_frames_after(calls: &[Call], marker: usize, wid: WindowId) -> usize {
        calls[marker..]
            .iter()
            .filter(|call| matches!(call, Call::SetFrame(w, _) if *w == wid))
            .count()
    }

    #[test]
    fn startup_adds_back_to_front_and_focuses_the_front_window() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        let report = core.startup(backend.windows());
        assert_eq!(report.added, 2);
        assert_eq!(core.layout().focused(), Some(1));
        let calls = backend.calls();
        assert!(calls.contains(&Call::Focus(1)), "{calls:?}");
        assert_eq!(core.layout().windows().len(), 2);
    }

    #[test]
    fn external_focus_of_a_parked_window_switches_workspace() {
        let (mut core, backend, _dir) = core(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        core.startup(backend.windows());
        core.action(Action::Workspace { index: 2 });
        assert_eq!(core.layout().active_workspace(), 2);
        assert!(!core.applier().is_visible(1));
        core.set_debounce(Duration::ZERO);

        core.platform_event(PlatformEvent::FocusChanged {
            wid: Some(1),
            pid: Some(42),
        });
        assert_eq!(core.layout().active_workspace(), 1);
        assert!(core.applier().is_visible(1));
        assert!(
            backend.calls().contains(&Call::Unpark(1)),
            "{:?}",
            backend.calls()
        );
    }

    #[test]
    fn user_move_of_a_tiled_window_snaps_back() {
        let original = Rect::new(0.0, 0.0, 400.0, 300.0);
        let (mut core, backend, _dir) = core(&[(1, original)]);
        core.startup(backend.windows());
        // The *move* debounce lives in the applier; the core's own debounce
        // only covers focus events.
        core.applier.set_debounce(Duration::ZERO);
        let layout_frame = core.layout().frame_of(1).unwrap();
        let moved = Rect::new(10.0, 10.0, 50.0, 50.0);
        backend.state.lock().unwrap().frames.insert(1, moved);
        let marker = backend.calls().len();
        core.platform_event_at(
            PlatformEvent::WindowMoved {
                wid: 1,
                frame: PlatformRect::new(moved.x, moved.y, moved.width, moved.height),
            },
            Instant::now(),
        );
        let calls = backend.calls();
        assert!(
            calls[marker..].contains(&Call::SetFrame(1, layout_frame)),
            "the window must be snapped back to its tile: {calls:?}"
        );
        assert_eq!(backend.frame(1), Some(layout_frame));
    }

    #[test]
    fn queued_move_captured_inside_the_debounce_is_an_echo() {
        let original = Rect::new(0.0, 0.0, 400.0, 300.0);
        let (mut core, backend, _dir) = core(&[(1, original)]);
        // Captured while the startup batch was still being applied.
        let queued = Instant::now();
        core.startup(backend.windows());
        core.applier.set_debounce(Duration::from_millis(500));
        // Captured right after the frame was programmed; processed whenever.
        let captured = Instant::now();
        let intermediate = PlatformRect::new(10.0, 10.0, 50.0, 50.0);
        let marker = backend.calls().len();

        core.platform_event_at(
            PlatformEvent::WindowMoved {
                wid: 1,
                frame: intermediate,
            },
            captured,
        );
        // Even once the debounce has expired by processing-time standards
        // (zero), an event captured *before* the frame was programmed is
        // stale, not a user drag.
        core.applier.set_debounce(Duration::ZERO);
        core.platform_event_at(
            PlatformEvent::WindowMoved {
                wid: 1,
                frame: intermediate,
            },
            queued,
        );

        let calls = backend.calls();
        assert!(
            calls[marker..].is_empty(),
            "a queued echo must not snap back or invalidate: {calls:?}"
        );
    }

    #[test]
    fn move_captured_after_the_debounce_is_a_user_move() {
        let original = Rect::new(0.0, 0.0, 400.0, 300.0);
        let (mut core, backend, _dir) = core(&[(1, original)]);
        core.startup(backend.windows());
        core.applier.set_debounce(Duration::from_millis(500));
        let layout_frame = core.layout().frame_of(1).unwrap();
        let moved = Rect::new(10.0, 10.0, 50.0, 50.0);
        backend.state.lock().unwrap().frames.insert(1, moved);
        // Processed well inside the debounce, but captured after it: the
        // capture time decides.
        let captured = Instant::now() + Duration::from_millis(600);
        let marker = backend.calls().len();

        core.platform_event_at(
            PlatformEvent::WindowMoved {
                wid: 1,
                frame: PlatformRect::new(moved.x, moved.y, moved.width, moved.height),
            },
            captured,
        );

        let calls = backend.calls();
        assert!(
            calls[marker..].contains(&Call::SetFrame(1, layout_frame)),
            "a move captured after the debounce must snap back: {calls:?}"
        );
        assert!(
            calls[marker..].contains(&Call::ForgetProbe(1)),
            "a user resize must invalidate the probe cache: {calls:?}"
        );
    }

    #[test]
    fn app_activation_is_judged_by_capture_time() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        let queued = Instant::now();
        core.startup(backend.windows());
        assert_eq!(core.layout().focused(), Some(1));
        let activated = PlatformEvent::AppActivated {
            pid: Some(test_pid(2)),
        };

        // Captured inside the focus debounce: part of our own focus burst.
        core.platform_event_at(activated.clone(), Instant::now());
        assert_eq!(core.layout().focused(), Some(1));

        // Captured before our focus change: stale even once the debounce is
        // over by processing-time standards.
        core.set_debounce(Duration::ZERO);
        core.platform_event_at(activated.clone(), queued);
        assert_eq!(core.layout().focused(), Some(1));

        // Captured after the debounce: the user activated the app.
        core.set_debounce(Duration::from_millis(250));
        core.platform_event_at(activated, Instant::now() + Duration::from_millis(300));
        assert_eq!(core.layout().focused(), Some(2));
    }

    #[test]
    fn startup_syncs_each_window_once_and_focuses_once() {
        const N: usize = 6;
        let frames: Vec<(WindowId, Rect)> = (1..=N as WindowId)
            .map(|wid| (wid, Rect::new(0.0, 0.0, 400.0, 300.0)))
            .collect();
        let (mut core, backend, _dir) = core(&frames);

        let report = core.startup(backend.windows());

        assert_eq!(report.added, N);
        let calls = backend.calls();
        let set_frames = calls
            .iter()
            .filter(|call| matches!(call, Call::SetFrame(..)))
            .count();
        assert!(
            set_frames <= N + 2,
            "startup must not re-apply every tile per insert: {set_frames} set_frame calls"
        );
        for wid in 1..=N as WindowId {
            assert!(
                core.applier().is_visible(wid),
                "window {wid} was not framed"
            );
            assert_eq!(backend.frame(wid), core.layout().frame_of(wid));
        }
        let focuses: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, call)| matches!(call, Call::Focus(_)))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(focuses.len(), 1, "exactly one focus call: {calls:?}");
        assert_eq!(calls[focuses[0]], Call::Focus(1));
        let last_frame = calls
            .iter()
            .rposition(|call| matches!(call, Call::SetFrame(..)))
            .expect("frames");
        assert!(
            focuses[0] > last_frame,
            "the focus must come after every frame: {calls:?}"
        );
        assert_eq!(core.layout().focused(), Some(1));
    }

    #[test]
    fn compact_keeps_the_last_frame_per_window_at_its_last_position() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(0.0, 0.0, 20.0, 20.0);
        let c = Rect::new(0.0, 0.0, 30.0, 30.0);
        let compacted = compact_commands(vec![
            Command::SetFrame { wid: 1, rect: a },
            Command::SetFrame { wid: 2, rect: b },
            Command::Show { wid: 3 },
            Command::SetFrame { wid: 1, rect: c },
            Command::SetFrame { wid: 3, rect: a },
        ]);
        assert_eq!(
            compacted,
            vec![
                Command::SetFrame { wid: 2, rect: b },
                Command::Show { wid: 3 },
                Command::SetFrame { wid: 1, rect: c },
                Command::SetFrame { wid: 3, rect: a },
            ]
        );
    }

    #[test]
    fn compact_keeps_only_the_final_focus_and_its_raise() {
        let compacted = compact_commands(vec![
            Command::Focus { wid: 1 },
            Command::Raise { wid: 1 },
            Command::Raise { wid: 2 },
            Command::Unfocus,
            Command::Focus { wid: 2 },
            Command::Raise { wid: 3 },
            Command::Raise { wid: 2 },
            Command::Focus { wid: 1 },
            Command::Raise { wid: 2 },
        ]);
        // The final focus is window 1; its last raise is kept where it was.
        assert_eq!(
            compacted,
            vec![Command::Raise { wid: 1 }, Command::Focus { wid: 1 }]
        );

        let compacted = compact_commands(vec![
            Command::Raise { wid: 2 },
            Command::Focus { wid: 1 },
            Command::Raise { wid: 1 },
            Command::Raise { wid: 2 },
            Command::Focus { wid: 2 },
            Command::Raise { wid: 3 },
        ]);
        assert_eq!(
            compacted,
            vec![Command::Raise { wid: 2 }, Command::Focus { wid: 2 }]
        );
    }

    #[test]
    fn compact_drops_every_raise_without_a_focus() {
        let compacted = compact_commands(vec![
            Command::Raise { wid: 1 },
            Command::Unfocus,
            Command::Raise { wid: 2 },
        ]);
        assert!(compacted.is_empty(), "{compacted:?}");
        assert!(compact_commands(Vec::new()).is_empty());
    }

    #[test]
    fn compact_keeps_visibility_and_workspace_commands_and_the_last_stack() {
        let stack = StackInfo {
            workspace: 1,
            tabs: Vec::new(),
            active: 0,
            frame: Rect::new(0.0, 0.0, 10.0, 10.0),
        };
        let compacted = compact_commands(vec![
            Command::ActivateWorkspace { index: 2 },
            Command::Hide { wid: 3 },
            Command::StackChanged { stack: None },
            Command::Show { wid: 4 },
            Command::Close { wid: 5 },
            Command::StackChanged {
                stack: Some(stack.clone()),
            },
            Command::Hide { wid: 4 },
            Command::ActivateWorkspace { index: 1 },
        ]);
        assert_eq!(
            compacted,
            vec![
                Command::ActivateWorkspace { index: 2 },
                Command::Hide { wid: 3 },
                Command::Show { wid: 4 },
                Command::Close { wid: 5 },
                Command::StackChanged { stack: Some(stack) },
                Command::Hide { wid: 4 },
                Command::ActivateWorkspace { index: 1 },
            ]
        );
    }

    #[test]
    fn resize_mode_routes_focus_actions_to_resize() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        core.action(Action::ResizeMode);
        assert!(core.layout().resize_mode());
        let focused = core.layout().focused();
        let before = core.layout().windows();
        core.action(Action::Focus {
            direction: cosmic_mac_protocol::Direction::Left,
        });
        let after = core.layout().windows();
        assert_eq!(
            core.layout().focused(),
            focused,
            "resize must not change focus"
        );
        assert_ne!(
            before.iter().map(|w| w.frame).collect::<Vec<_>>(),
            after.iter().map(|w| w.frame).collect::<Vec<_>>(),
            "resize must change frames"
        );
        assert!(core.layout().resize_mode());
    }

    #[test]
    fn ipc_queries_are_answered_from_the_layout_snapshot() {
        let (mut core, backend, _dir) = core(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        core.startup(backend.windows());
        assert!(matches!(
            core.request(Request::ListWindows),
            Response::Windows(windows) if windows.len() == 1
        ));
        assert!(matches!(
            core.request(Request::ListWorkspaces),
            Response::Workspaces(workspaces) if workspaces[0].active
        ));
        assert!(matches!(
            core.request(Request::Tree),
            Response::Tree(Some(_))
        ));
        assert!(matches!(core.request(Request::Config), Response::Config(_)));
        assert_eq!(core.request(Request::Action(Action::Close)), Response::Ok);
    }

    #[test]
    fn title_changes_reach_the_layout() {
        let (mut core, backend, _dir) = core(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        core.startup(backend.windows());
        core.platform_event(PlatformEvent::WindowTitleChanged {
            wid: 1,
            title: Some("Renamed".into()),
        });
        assert_eq!(
            core.layout().window_info(1).map(|info| info.title.as_str()),
            Some("Renamed")
        );
    }

    #[test]
    fn clamped_frame_triggers_probe_retile_and_flush_tiles() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        assert_eq!(core.constraint_stats().probes, 0);

        // Notes-style horizontal minimum: the app refuses to go below 600px.
        backend.set_min_size(1, Size::new(600.0, 100.0));
        let marker = backend.calls().len();
        core.set_work_area(Rect::new(0.0, 0.0, 900.0, 800.0));

        let stats = core.constraint_stats();
        assert_eq!(stats.probes, 1, "the clamp must trigger exactly one probe");
        assert_eq!(
            stats.retiles, 1,
            "the probe must trigger exactly one retile"
        );
        assert_eq!(
            stats.warnings, 0,
            "the layout absorbed the minimum, so nothing is stuck"
        );
        assert_eq!(
            core.constraints(1).and_then(|constraints| constraints.min),
            Some(Size::new(600.0, 100.0)),
            "the learned minimum is cached"
        );
        // The re-apply step issues the layout tile again after the probe.
        assert!(
            set_frames_after(&backend.calls(), marker, 1) >= 2,
            "expected the clamped window to be re-applied: {:?}",
            backend.calls()
        );

        // The re-tile is flush: the clamped window takes its minimum and the
        // sibling shrinks to absorb it.
        let frames = backend.frames();
        assert_eq!(frames.get(&1).map(|frame| frame.width), Some(600.0));
        assert_eq!(frames.get(&2).map(|frame| frame.width), Some(300.0));
        assert!(
            (frames.get(&1).unwrap().area() + frames.get(&2).unwrap().area() - 900.0 * 800.0).abs()
                < 1.0
        );

        // A later layout change must not probe again.
        core.set_work_area(Rect::new(0.0, 0.0, 1000.0, 800.0));
        assert_eq!(core.constraint_stats().probes, 1);
        assert_eq!(backend.probes(), 1);
        assert_eq!(core.constraint_stats().warnings, 0);
    }

    #[test]
    fn a_frozen_dimension_is_pinned_and_absorbed_by_the_siblings() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());

        // System Settings freezes its width at 715px: it ignores both larger
        // and smaller requests, and the platform classifies the axis as Frozen.
        backend.simulate_frozen_width(1, 715.0);
        core.set_work_area(Rect::new(0.0, 0.0, 900.0, 800.0));

        assert_eq!(core.constraint_stats().probes, 1);
        let constraints = core.constraints(1).expect("constraints");
        assert_eq!(constraints.min.map(|size| size.width), Some(715.0));
        assert_eq!(constraints.max.map(|size| size.width), Some(715.0));
        // The layout asked for exactly the frozen width on the re-tile and the
        // sibling absorbed the difference: no warning, no wallpaper gap.
        assert_eq!(core.constraint_stats().warnings, 0);
        let frames = backend.frames();
        assert_eq!(frames.get(&1).map(|frame| frame.width), Some(715.0));
        assert_eq!(frames.get(&2).map(|frame| frame.width), Some(185.0));
    }

    #[test]
    fn grid_snapping_terminals_are_never_pinned_or_centred() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        // Two Terminals side by side: 7×14px character cells (a 900×500 tile
        // comes back as 896×504) and a 20×5 cell minimum.
        for wid in [1, 2] {
            backend.simulate_grid(wid, Size::new(7.0, 14.0));
            backend.set_min_size(wid, Size::new(140.0, 70.0));
        }

        // The 900×500 tiles come back between other layouts, and once with
        // requests that round to the same cells.
        for (width, height) in [
            (1800.0, 500.0),
            (1900.0, 520.0),
            (1800.0, 500.0),
            (2000.0, 560.0),
            (1804.0, 497.0),
            (1800.0, 500.0),
        ] {
            core.set_work_area(Rect::new(0.0, 0.0, width, height));
            // Flush tiles: each Terminal keeps half the width and the full
            // height, so nothing is centred with gaps.
            for wid in [1, 2] {
                let tile = core.layout().frame_of(wid).expect("tile");
                assert_eq!(
                    (tile.width, tile.height),
                    (width / 2.0, height),
                    "window {wid} in a {width}x{height} work area"
                );
            }
        }

        assert_eq!(
            backend.frame(1).map(|frame| (frame.width, frame.height)),
            Some((896.0, 504.0))
        );
        assert_eq!(core.constraint_stats().probes, 2, "one probe per window");
        for wid in [1, 2] {
            let constraints = core.constraints(wid).expect("constraints");
            assert!(constraints.confirmed);
            assert_eq!(
                constraints.max, None,
                "window {wid}: a snap pinned a maximum"
            );
            assert_eq!(
                constraints.min,
                Some(Size::new(140.0, 70.0)),
                "window {wid}: a snap raised the minimum"
            );
            let info = core.layout().window_info(wid).expect("window info");
            assert_eq!(info.max_size, None);
            assert_eq!(info.min_size, Some(Size::new(140.0, 70.0)));
        }
    }

    #[test]
    fn title_bar_drag_of_a_grid_snapped_window_keeps_its_constraints() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        for wid in [1, 2] {
            backend.simulate_grid(wid, Size::new(7.0, 14.0));
            backend.set_min_size(wid, Size::new(140.0, 70.0));
        }
        core.set_work_area(Rect::new(0.0, 0.0, 1800.0, 500.0));
        let learned = core.constraints(1).expect("constraints after the clamp");
        let probes = core.constraint_stats().probes;
        let snapped = backend.frame(1).expect("frame");
        assert_ne!(
            (snapped.width, snapped.height),
            (900.0, 500.0),
            "the grid must leave the window off its tile"
        );
        core.applier.set_debounce(Duration::ZERO);

        // A title-bar drag: same (snapped) size, new position.
        let dragged = Rect::new(
            snapped.x + 40.0,
            snapped.y + 30.0,
            snapped.width,
            snapped.height,
        );
        backend.state.lock().unwrap().frames.insert(1, dragged);
        core.platform_event_at(
            PlatformEvent::WindowMoved {
                wid: 1,
                frame: PlatformRect::new(dragged.x, dragged.y, dragged.width, dragged.height),
            },
            Instant::now(),
        );

        // A resize would drop the learned bounds, and the snap-back would then
        // clamp and probe the window again.
        assert_eq!(
            core.constraint_stats().probes,
            probes,
            "a pure move must not count as a user resize"
        );
        assert_eq!(core.constraints(1), Some(learned));
    }

    #[test]
    fn a_real_maximum_the_probe_missed_is_learned_when_it_repeats() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        // The app caps its width at 800px, but the window is already wider
        // than the grow probe's display cap, so the probe cannot see it.
        backend.set_max_size(1, Size::new(800.0, f64::MAX), false);

        core.set_work_area(Rect::new(0.0, 0.0, 1800.0, 800.0));
        assert_eq!(core.constraint_stats().probes, 1);
        assert_eq!(
            core.constraints(1).and_then(|constraints| constraints.max),
            None,
            "one clamp is not a bound"
        );

        // A different request stops at the same 800px: that is a bound.
        core.set_work_area(Rect::new(0.0, 0.0, 2000.0, 800.0));
        assert_eq!(core.constraint_stats().probes, 1, "no re-probe");
        assert_eq!(
            core.constraints(1).and_then(|constraints| constraints.max),
            Some(Size::new(800.0, f64::MAX))
        );
        assert_eq!(
            core.layout().window_info(1).and_then(|info| info.max_size),
            Some(Size::new(800.0, f64::MAX))
        );
        // The sibling absorbs the rest: flush, no gap.
        assert_eq!(backend.frame(1).map(|frame| frame.width), Some(800.0));
        assert_eq!(backend.frame(2).map(|frame| frame.width), Some(1200.0));
    }

    #[test]
    fn a_maximum_confirmed_by_the_probe_is_learned_from_the_first_clamp() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        backend.set_max_size(1, Size::new(800.0, f64::MAX), true);

        core.set_work_area(Rect::new(0.0, 0.0, 1800.0, 800.0));

        assert_eq!(core.constraint_stats().probes, 1);
        assert_eq!(
            core.constraints(1).and_then(|constraints| constraints.max),
            Some(Size::new(800.0, f64::MAX))
        );
        assert_eq!(core.constraint_stats().warnings, 0, "the re-tile is flush");
        assert_eq!(backend.frame(1).map(|frame| frame.width), Some(800.0));
        assert_eq!(backend.frame(2).map(|frame| frame.width), Some(1000.0));
    }

    #[test]
    fn list_windows_reports_the_actual_frame_next_to_the_layout_frame() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        // The app refuses to leave 715px and the probe is unavailable, so the
        // layout keeps its 700px tile while the real frame stays 715px wide.
        backend.simulate_frozen_width(1, 715.0);
        backend.fail_probes();
        core.set_work_area(Rect::new(0.0, 0.0, 1400.0, 800.0));

        let Response::Windows(windows) = core.request(Request::ListWindows) else {
            panic!("expected a window list");
        };
        assert_eq!(windows.len(), 2);
        for state in &windows {
            assert_eq!(
                state.actual_frame,
                backend.frame(state.info.wid),
                "window {} must report the backend frame",
                state.info.wid
            );
        }
        let refusing = windows.iter().find(|state| state.info.wid == 1).unwrap();
        let actual = refusing.actual_frame.expect("actual frame");
        assert!((actual.width - 715.0).abs() < 1.0);
        assert!(
            !actual.approx_eq(refusing.frame, 1.0),
            "the frozen window should differ from its layout frame: {actual:?} vs {:?}",
            refusing.frame
        );
    }

    #[test]
    fn a_single_clamp_with_a_failed_probe_never_pins_a_bound() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());

        // Worst case: the app is stuck at 715px and the probe is unavailable.
        // The clamp direction only proves it would not shrink, so it must be
        // kept as a candidate, never as an authoritative minimum/maximum.
        backend.simulate_frozen_width(1, 715.0);
        backend.fail_probes();
        let marker = backend.calls().len();
        core.set_work_area(Rect::new(0.0, 0.0, 900.0, 800.0));

        let stats = core.constraint_stats();
        assert_eq!(stats.probes, 1);
        assert_eq!(stats.retiles, 1);
        assert_eq!(stats.warnings, 1, "the stuck dimension is reported once");
        assert!(
            backend.calls().len() - marker < 12,
            "the first clamp pass must be bounded: {} calls",
            backend.calls().len() - marker
        );

        let constraints = core.constraints(1).expect("constraints");
        assert!(!constraints.probed, "a transient failure stays retryable");
        assert_eq!(constraints.probe_attempts, 1);
        assert!(!constraints.confirmed);
        assert_eq!(
            constraints.min, None,
            "an unconfirmed clamp must not pin a min"
        );
        assert_eq!(
            constraints.max, None,
            "a Min candidate must not invent a max"
        );
        assert_eq!(constraints.candidate_min.width(), Some(715.0));
        assert!(constraints.candidate_max.width().is_none());

        // Further layout passes retry the probe up to the cap, then stop; the
        // stuck warning is not repeated.
        let after_first = backend.calls().len();
        let max_attempts = usize::from(LearnedConstraints::MAX_PROBE_ATTEMPTS);
        core.set_work_area(Rect::new(0.0, 0.0, 1200.0, 900.0));
        core.set_work_area(Rect::new(0.0, 0.0, 1000.0, 700.0));
        core.set_work_area(Rect::new(0.0, 0.0, 1100.0, 800.0));
        core.set_work_area(Rect::new(0.0, 0.0, 1300.0, 900.0));
        assert_eq!(core.constraint_stats().probes, max_attempts);
        assert_eq!(core.constraint_stats().warnings, 1);
        assert_eq!(backend.probes(), max_attempts);
        assert!(
            backend.calls().len() - after_first < 40,
            "no retry storm: {} extra calls",
            backend.calls().len() - after_first
        );

        let constraints = core.constraints(1).expect("constraints");
        assert!(constraints.probed, "the probe gave up at the cap");
        assert_eq!(
            constraints.probe_attempts,
            LearnedConstraints::MAX_PROBE_ATTEMPTS
        );
        assert!(!constraints.confirmed);
        assert_eq!(constraints.min, None);
        assert_eq!(constraints.max, None);
        assert_eq!(constraints.candidate_min.width(), Some(715.0));
    }

    #[test]
    fn probe_failures_are_tolerated_and_do_not_loop() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        // A clamped-but-shrinkable app whose probe is unavailable: the daemon
        // records the directional candidate, warns once, and never pins it as
        // authoritative without the probe's confirmation.
        backend.set_min_size(1, Size::new(700.0, 100.0));
        backend.fail_probes();
        let marker = backend.calls().len();

        core.set_work_area(Rect::new(0.0, 0.0, 900.0, 800.0));

        let stats = core.constraint_stats();
        assert_eq!(stats.probes, 1);
        assert_eq!(stats.retiles, 1);
        assert_eq!(stats.warnings, 1);
        // Bounded: one initial application plus one re-apply for the window.
        assert!(set_frames_after(&backend.calls(), marker, 1) <= 3);
        let constraints = core.constraints(1).expect("constraints");
        assert!(!constraints.probed, "a transient failure stays retryable");
        assert_eq!(constraints.probe_attempts, 1);

        // Later layout passes retry until the cap, then stop probing.
        let max_attempts = usize::from(LearnedConstraints::MAX_PROBE_ATTEMPTS);
        let after_first = backend.calls().len();
        for width in [1000.0, 1100.0, 1200.0, 1300.0, 1400.0] {
            core.set_work_area(Rect::new(0.0, 0.0, width, 800.0));
        }
        assert_eq!(core.constraint_stats().probes, max_attempts);
        assert_eq!(backend.probes(), max_attempts);
        assert_eq!(core.constraint_stats().warnings, 1);
        assert!(
            backend.calls().len() - after_first < 60,
            "no retry storm: {} extra calls",
            backend.calls().len() - after_first
        );
        let constraints = core.constraints(1).expect("constraints");
        assert!(constraints.probed);
        assert!(!constraints.confirmed);
        assert_eq!(constraints.min, None);
        assert_eq!(constraints.max, None);
        assert_eq!(constraints.candidate_min.width(), Some(700.0));
    }

    #[test]
    fn transient_probe_failure_is_retried_and_then_learns() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        backend.set_min_size(1, Size::new(600.0, 100.0));
        backend.fail_probes();

        core.set_work_area(Rect::new(0.0, 0.0, 900.0, 800.0));
        assert_eq!(backend.probes(), 1);
        let constraints = core.constraints(1).expect("constraints");
        assert_eq!(constraints.probe_attempts, 1);
        assert!(!constraints.probed);
        assert!(!constraints.confirmed);

        backend.allow_probes();
        core.set_work_area(Rect::new(0.0, 0.0, 1000.0, 800.0));
        assert_eq!(backend.probes(), 2);
        assert_eq!(core.constraint_stats().probes, 2);
        let constraints = core.constraints(1).expect("constraints");
        assert!(constraints.probed);
        assert!(constraints.confirmed);
        assert_eq!(constraints.min, Some(Size::new(600.0, 100.0)));

        // Learned: no further probes.
        core.set_work_area(Rect::new(0.0, 0.0, 1100.0, 800.0));
        assert_eq!(backend.probes(), 2);
    }

    #[test]
    fn user_resize_invalidates_learned_constraints_and_reprobes() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        core.set_debounce(Duration::ZERO);
        backend.set_min_size(1, Size::new(600.0, 100.0));

        core.set_work_area(Rect::new(0.0, 0.0, 900.0, 800.0));
        assert_eq!(core.constraint_stats().probes, 1);
        assert_eq!(
            core.constraints(1).and_then(|constraints| constraints.min),
            Some(Size::new(600.0, 100.0))
        );

        // A user edge-resize on a tiled window is not an echo: the learned
        // bounds and the platform probe cache must be dropped.  The app's
        // minimum changed with it.
        backend.set_min_size(1, Size::new(700.0, 100.0));
        core.applier.set_debounce(Duration::ZERO);
        let before = backend.calls().len();
        core.platform_event(PlatformEvent::WindowMoved {
            wid: 1,
            frame: PlatformRect::new(10.0, 10.0, 700.0, 500.0),
        });
        let calls = backend.calls();
        assert!(
            calls[before..].contains(&Call::ForgetProbe(1)),
            "platform probe cache must be invalidated: {calls:?}"
        );
        // Clearing the layout's bounds re-tiles the window below the app's
        // minimum; that clamp re-probes at once and learns the new minimum.
        assert_eq!(core.constraint_stats().probes, 2);
        assert_eq!(
            core.constraints(1).and_then(|constraints| constraints.min),
            Some(Size::new(700.0, 100.0))
        );
        assert_eq!(
            core.layout().window_info(1).and_then(|info| info.min_size),
            Some(Size::new(700.0, 100.0)),
            "the re-learned minimum reaches the layout"
        );
        assert_eq!(backend.frame(1).map(|frame| frame.width), Some(700.0));
        assert_eq!(backend.frame(2).map(|frame| frame.width), Some(200.0));

        // Learned again: a later layout change does not probe.
        core.set_work_area(Rect::new(0.0, 0.0, 1000.0, 800.0));
        assert_eq!(core.constraint_stats().probes, 2);
        assert_eq!(
            core.constraints(1).and_then(|constraints| constraints.min),
            Some(Size::new(700.0, 100.0))
        );
    }

    #[test]
    fn user_resize_clears_the_bounds_held_by_the_layout() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        backend.set_min_size(1, Size::new(600.0, 100.0));
        core.set_work_area(Rect::new(0.0, 0.0, 900.0, 800.0));
        assert_eq!(
            core.layout().window_info(1).and_then(|info| info.min_size),
            Some(Size::new(600.0, 100.0)),
            "the probe pinned a minimum in the layout"
        );
        assert_eq!(
            core.layout().frame_of(1).map(|rect| rect.width),
            Some(600.0)
        );

        // The user resizes the window and the app no longer insists on 600px.
        backend.set_min_size(1, Size::new(0.0, 0.0));
        core.applier.set_debounce(Duration::ZERO);
        core.platform_event(PlatformEvent::WindowResized {
            wid: 1,
            frame: PlatformRect::new(0.0, 0.0, 700.0, 800.0),
        });

        assert!(core.constraints(1).is_none());
        let info = core.layout().window_info(1).expect("window info");
        assert_eq!(info.min_size, None, "stale minimum still shapes the tiles");
        assert_eq!(info.max_size, None);
        // The tiles are recomputed without the stale bound.
        assert_eq!(
            core.layout().frame_of(1).map(|rect| rect.width),
            Some(450.0)
        );
        assert_eq!(backend.frame(1).map(|frame| frame.width), Some(450.0));
        assert_eq!(backend.frame(2).map(|frame| frame.width), Some(450.0));
        assert_eq!(core.constraint_stats().probes, 1);
    }

    #[test]
    fn position_only_move_keeps_learned_constraints() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        backend.set_min_size(1, Size::new(600.0, 100.0));
        core.set_work_area(Rect::new(0.0, 0.0, 900.0, 800.0));
        let learned = core.constraints(1).expect("constraints");
        assert_eq!(learned.min, Some(Size::new(600.0, 100.0)));
        let tile = core.layout().frame_of(1).expect("tile");

        // A title-bar drag: same size, different position.
        core.applier.set_debounce(Duration::ZERO);
        let marker = backend.calls().len();
        core.platform_event(PlatformEvent::WindowMoved {
            wid: 1,
            frame: PlatformRect::new(tile.x + 40.0, tile.y + 30.0, tile.width, tile.height),
        });

        assert_eq!(
            core.constraints(1),
            Some(learned),
            "a good probe was dropped"
        );
        let calls = backend.calls();
        assert!(
            !calls[marker..].contains(&Call::ForgetProbe(1)),
            "a move must not invalidate the probe cache: {calls:?}"
        );
        assert_eq!(
            core.layout().window_info(1).and_then(|info| info.min_size),
            Some(Size::new(600.0, 100.0))
        );
        // The tiled window still snaps back to its slot.
        assert_eq!(backend.frame(1), Some(tile));
        assert_eq!(core.constraint_stats().probes, 1);
    }

    #[test]
    fn user_move_of_a_floating_window_is_adopted_by_the_layout() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        core.action(Action::ToggleWindowFloating);
        let focused = core.layout().focused().expect("focused window");
        let old = core.layout().frame_of(focused).expect("floating frame");
        assert!(
            core.layout()
                .windows()
                .iter()
                .any(|state| state.info.wid == focused && state.floating)
        );

        // The user drags the floating window somewhere else (not an echo).
        core.set_debounce(Duration::ZERO);
        core.applier.set_debounce(Duration::ZERO);
        let moved = Rect::new(old.x + 50.0, old.y + 60.0, old.width, old.height);
        backend.state.lock().unwrap().frames.insert(focused, moved);
        let marker = backend.calls().len();
        core.platform_event(PlatformEvent::WindowMoved {
            wid: focused as u32,
            frame: PlatformRect::new(moved.x, moved.y, moved.width, moved.height),
        });

        assert_eq!(core.layout().frame_of(focused), Some(moved));
        let state = core
            .layout()
            .windows()
            .into_iter()
            .find(|state| state.info.wid == focused)
            .unwrap();
        assert_eq!(state.frame, moved);
        assert_eq!(
            set_frames_after(&backend.calls(), marker, focused),
            0,
            "the window must not be snapped back: {:?}",
            backend.calls()
        );

        // A workspace round trip unparks it and must not re-apply the old frame.
        core.action(Action::Workspace { index: 2 });
        core.action(Action::Workspace { index: 1 });
        let calls = backend.calls();
        assert!(
            calls[marker..].contains(&Call::Unpark(focused)),
            "{calls:?}"
        );
        assert!(
            !calls[marker..].contains(&Call::SetFrame(focused, old)),
            "the stale placement was re-emitted: {calls:?}"
        );
        assert_eq!(backend.frame(focused), Some(moved));
        assert_eq!(core.layout().frame_of(focused), Some(moved));
    }

    #[test]
    fn restore_hides_the_stack_tab_bar() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        core.action(Action::ToggleStacking);
        assert!(
            core.layout().active_stack().is_some(),
            "windows are stacked"
        );
        assert!(
            backend
                .calls()
                .iter()
                .any(|call| matches!(call, Call::StackBar(Some(_)))),
            "{:?}",
            backend.calls()
        );

        let marker = backend.calls().len();
        assert_eq!(core.request(Request::Restore), Response::Ok);
        let calls = backend.calls();
        assert!(
            calls[marker..].contains(&Call::StackBar(None)),
            "the tab bar must be hidden after restore: {calls:?}"
        );
        assert!(core.layout().windows().is_empty());
    }

    #[test]
    fn a_stale_journal_entry_survives_startup_and_is_restored_at_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("frame-journal.ron");
        let original = Rect::new(10.0, 20.0, 400.0, 300.0);
        // The crashed run's replay failed for window 1, which is still where
        // parking left it on the 1920×1080 display.
        Journal::new(&journal_path).record(1, original, Some(test_pid(1)));
        let parked = Rect::new(1919.0, 1079.0, 400.0, 300.0);
        let backend = Arc::new(TestBackend::new(&[(1, parked)], WORK_AREA));
        let mut core = Core::new(
            Config::default(),
            WORK_AREA,
            backend.clone(),
            journal_path.clone(),
            dir.path().join("wm-session.ron"),
        );

        core.startup(backend.windows());
        assert_ne!(backend.frame(1), Some(parked), "the layout tiles it");
        assert_eq!(core.applier().journal().get(1), Some(original));

        core.shutdown();
        assert_eq!(backend.frame(1), Some(original));
        assert!(!journal_path.exists());
    }

    #[test]
    fn work_area_change_reframes_every_window() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (3, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        let marker = backend.calls().len();

        let area = Rect::new(0.0, 0.0, 1440.0, 900.0);
        core.set_work_area(area);

        let calls = backend.calls();
        let mut covered = 0.0;
        for wid in [1, 2, 3] {
            assert_eq!(
                set_frames_after(&calls, marker, wid),
                1,
                "window {wid} must be re-framed exactly once"
            );
            let frame = backend.frame(wid).expect("frame");
            assert!(
                frame.x >= area.x - 0.5 && frame.y >= area.y - 0.5,
                "{frame:?}"
            );
            assert!(
                frame.right() <= area.right() + 0.5 && frame.bottom() <= area.bottom() + 0.5,
                "{frame:?} outside {area:?}"
            );
            covered += frame.area();
        }
        // Flush tiles: the frames cover the whole new work area.
        assert!(
            (covered - area.area()).abs() < 1.0,
            "{covered} != {}",
            area.area()
        );
        assert_eq!(core.applier().work_area(), area);
    }

    #[test]
    fn restart_with_a_changed_work_area_rebuilds_a_fresh_tree() {
        let new_area = Rect::new(0.0, 0.0, 1600.0, 1000.0);
        let session = SessionState {
            work_area: Some(WORK_AREA),
            tiled_order: vec![6, 5, 4, 3, 2, 1],
        };
        let frames: Vec<(WindowId, Rect)> = (1..=6)
            .map(|wid| (wid, Rect::new(0.0, 0.0, 400.0, 300.0)))
            .collect();
        let (mut core, backend, _dir) = core_with(&frames, new_area, Some(session));

        let report = core.startup((1..=6).map(window).collect());

        assert!(
            report.tree_rebuilt,
            "a changed work area must discard the tree"
        );
        assert_eq!(report.order, vec![1, 2, 3, 4, 5, 6]);
        let frames = backend.frames();
        let mut smallest = f64::INFINITY;
        for wid in 1..=6 {
            let frame = frames.get(&wid).copied().expect("frame");
            assert!(
                frame.x >= new_area.x - 0.5
                    && frame.y >= new_area.y - 0.5
                    && frame.right() <= new_area.right() + 0.5
                    && frame.bottom() <= new_area.bottom() + 0.5,
                "window {wid} frame {frame:?} outside {new_area:?}"
            );
            smallest = smallest.min(frame.area());
        }
        // A balanced tree keeps the smallest tile within a factor of two of the
        // others; the old insertion order produced 1/64 tiles for the first
        // window.
        assert!(
            smallest >= new_area.area() / 8.0 - 1.0,
            "startup tree is not balanced: smallest tile {smallest} of {}",
            new_area.area()
        );
        // The session file now records the new work area.
        let recorded = SessionStore::new(_dir.path().join("wm-session.ron"))
            .load()
            .expect("session");
        assert_eq!(recorded.work_area, Some(new_area));
    }

    #[test]
    fn restart_with_an_unchanged_work_area_reuses_the_recorded_order() {
        let session = SessionState {
            work_area: Some(WORK_AREA),
            tiled_order: vec![2, 3, 1],
        };
        let frames: Vec<(WindowId, Rect)> = (1..=3)
            .map(|wid| (wid, Rect::new(0.0, 0.0, 400.0, 300.0)))
            .collect();
        let (mut core, _backend, _dir) = core_with(&frames, WORK_AREA, Some(session));

        let report = core.startup((1..=3).map(window).collect());

        assert!(!report.tree_rebuilt);
        assert_eq!(report.order, vec![2, 3, 1]);
    }

    #[test]
    fn shutdown_persists_the_session() {
        let (mut core, backend, dir) = core(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        core.startup(backend.windows());
        core.shutdown();
        let recorded = SessionStore::new(dir.path().join("wm-session.ron"))
            .load()
            .expect("session");
        assert_eq!(recorded.work_area, Some(WORK_AREA));
        assert_eq!(recorded.tiled_order, vec![1]);
    }

    /// The panel's window id in the shell-window tests.
    const PANEL: WindowId = 10;
    /// The panel at the top edge of [`WORK_AREA`], full width.
    const PANEL_STRIP: Rect = Rect::new(0.0, 0.0, 1000.0, 32.0);
    /// [`WORK_AREA`] minus [`PANEL_STRIP`].
    const RESERVED: Rect = Rect::new(0.0, 32.0, 1000.0, 768.0);

    fn platform_rect(rect: Rect) -> PlatformRect {
        PlatformRect::new(rect.x, rect.y, rect.width, rect.height)
    }

    fn created(wid: WindowId) -> PlatformEvent {
        PlatformEvent::WindowCreated {
            wid: wid as u32,
            pid: test_pid(wid),
        }
    }

    fn recorded_work_area(dir: &tempfile::TempDir) -> Option<Rect> {
        SessionStore::new(dir.path().join("wm-session.ron"))
            .load()
            .and_then(|state| state.work_area)
    }

    /// Every window in `wids` is framed inside `area`, and together they tile
    /// all of it.
    fn assert_tiles_cover(backend: &TestBackend, wids: &[WindowId], area: Rect) {
        let mut covered = 0.0;
        for &wid in wids {
            let frame = backend.frame(wid).expect("frame");
            assert!(
                frame.x >= area.x - 0.5
                    && frame.y >= area.y - 0.5
                    && frame.right() <= area.right() + 0.5
                    && frame.bottom() <= area.bottom() + 0.5,
                "window {wid} frame {frame:?} outside {area:?}"
            );
            covered += frame.area();
        }
        assert!(
            (covered - area.area()).abs() < 1.0,
            "tiles cover {covered}, not {} of {area:?}",
            area.area()
        );
    }

    #[test]
    fn window_created_above_the_normal_level_is_not_managed() {
        let (mut core, backend, _dir) = core(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        core.startup(backend.windows());
        // A Spotlight-like overlay at a raised level.
        backend.add_window(2, Rect::new(200.0, 150.0, 600.0, 60.0));
        backend.set_level(2, 25);
        let marker = backend.calls().len();

        core.platform_event(created(2));

        assert!(core.layout().window_info(2).is_none());
        let calls = backend.calls();
        assert!(calls[marker..].is_empty(), "{calls:?}");
        assert_eq!(backend.frame(1), Some(WORK_AREA));

        // A normal window created next is still managed.
        backend.add_window(3, Rect::new(0.0, 0.0, 400.0, 300.0));
        core.platform_event(created(3));
        assert!(core.layout().window_info(3).is_some());
    }

    #[test]
    fn minimized_window_created_later_is_not_managed() {
        let (mut core, backend, _dir) = core(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        core.startup(backend.windows());
        backend.add_window(2, Rect::new(0.0, 0.0, 400.0, 300.0));
        backend.set_minimized(2, true);
        let marker = backend.calls().len();

        core.platform_event(created(2));

        assert!(core.layout().window_info(2).is_none());
        let calls = backend.calls();
        assert!(calls[marker..].is_empty(), "{calls:?}");
        assert_eq!(backend.frame(1), Some(WORK_AREA));
    }

    #[test]
    fn startup_skips_raised_and_minimized_windows() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (3, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        backend.set_level(2, 3);
        backend.set_minimized(3, true);

        let report = core.startup(backend.windows());

        assert_eq!(report.added, 1);
        assert_eq!(report.order, vec![1]);
        assert_eq!(backend.frame(1), Some(WORK_AREA));
    }

    #[test]
    fn managed_window_leaving_the_normal_level_is_removed_and_the_rest_reflow() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        assert_tiles_cover(&backend, &[1, 2], WORK_AREA);
        assert!(core.applier().journal().contains(2));

        // A level event that leaves the window at level 0 changes nothing.
        let marker = backend.calls().len();
        core.platform_event(PlatformEvent::WindowLevelChanged { wid: 2 });
        assert!(core.layout().window_info(2).is_some());
        let calls = backend.calls();
        assert!(calls[marker..].is_empty(), "{calls:?}");

        // The app turns window 2 into a floating ("always on top") panel.
        backend.set_level(2, 3);
        core.platform_event(PlatformEvent::WindowLevelChanged { wid: 2 });

        assert!(core.layout().window_info(2).is_none());
        assert!(!core.applier().journal().contains(2));
        assert!(!core.applier().is_visible(2));
        let calls = backend.calls();
        assert_eq!(set_frames_after(&calls, marker, 2), 0, "{calls:?}");
        assert_eq!(
            set_frames_after(&calls, marker, 1),
            1,
            "the remaining tile must re-flow: {calls:?}"
        );
        assert_tiles_cover(&backend, &[1], WORK_AREA);

        // Back at level 0 it is not picked up again.
        backend.set_level(2, 0);
        core.platform_event(PlatformEvent::WindowLevelChanged { wid: 2 });
        assert!(core.layout().window_info(2).is_none());
    }

    #[test]
    fn parked_window_leaving_the_normal_level_is_unparked_first() {
        let (mut core, backend, _dir) = core(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        core.startup(backend.windows());
        core.action(Action::Workspace { index: 2 });
        assert!(!core.applier().is_visible(1));
        backend.set_level(1, 3);
        let marker = backend.calls().len();

        core.platform_event(PlatformEvent::WindowLevelChanged { wid: 1 });

        let calls = backend.calls();
        assert!(calls[marker..].contains(&Call::Unpark(1)), "{calls:?}");
        assert!(core.layout().window_info(1).is_none());
    }

    #[test]
    fn level_change_of_an_unmanaged_window_is_ignored() {
        let (mut core, backend, _dir) = core(&[(1, Rect::new(0.0, 0.0, 400.0, 300.0))]);
        core.startup(backend.windows());
        backend.add_window(2, Rect::new(0.0, 0.0, 400.0, 300.0));
        backend.set_level(2, 25);
        let marker = backend.calls().len();

        core.platform_event(PlatformEvent::WindowLevelChanged { wid: 2 });

        let calls = backend.calls();
        assert!(calls[marker..].is_empty(), "{calls:?}");
        assert!(core.layout().window_info(1).is_some());
    }

    #[test]
    fn panel_window_reserves_its_strip_until_it_closes() {
        let (mut core, backend, dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        // winit creates the panel at the normal level and a default position;
        // the panel raises it and moves it to the top edge a moment later.
        backend.add_window(PANEL, Rect::new(100.0, 384.0, 1000.0, 32.0));
        backend.set_app_id(PANEL, "cosmic-mac-panel");
        let marker = backend.calls().len();

        core.platform_event(created(PANEL));

        assert!(
            core.layout().window_info(PANEL).is_none(),
            "the panel is never managed"
        );
        // Mid-screen, it reserves nothing yet.
        assert_eq!(core.applier().work_area(), WORK_AREA);
        let calls = backend.calls();
        assert!(calls[marker..].is_empty(), "{calls:?}");

        // A failed bounds read (empty frame) is not a move.
        core.platform_event(PlatformEvent::WindowMoved {
            wid: PANEL as u32,
            frame: PlatformRect::default(),
        });
        assert_eq!(core.applier().work_area(), WORK_AREA);

        backend.set_level(PANEL, 25);
        core.platform_event(PlatformEvent::WindowLevelChanged { wid: PANEL as u32 });
        backend
            .state
            .lock()
            .unwrap()
            .frames
            .insert(PANEL, PANEL_STRIP);
        let marker = backend.calls().len();
        core.platform_event(PlatformEvent::WindowMoved {
            wid: PANEL as u32,
            frame: platform_rect(PANEL_STRIP),
        });

        assert_eq!(core.applier().work_area(), RESERVED);
        assert_eq!(core.layout().work_area(), RESERVED);
        let calls = backend.calls();
        assert_eq!(set_frames_after(&calls, marker, PANEL), 0, "{calls:?}");
        for wid in [1, 2] {
            assert_eq!(
                set_frames_after(&calls, marker, wid),
                1,
                "window {wid} must be re-framed below the panel: {calls:?}"
            );
        }
        assert_tiles_cover(&backend, &[1, 2], RESERVED);
        assert_eq!(recorded_work_area(&dir), Some(RESERVED));

        // The panel quits: the strip is released.
        backend.remove_window(PANEL);
        let marker = backend.calls().len();
        core.platform_event(PlatformEvent::WindowDestroyed { wid: PANEL as u32 });

        assert_eq!(core.applier().work_area(), WORK_AREA);
        assert_eq!(core.layout().work_area(), WORK_AREA);
        let calls = backend.calls();
        for wid in [1, 2] {
            assert_eq!(set_frames_after(&calls, marker, wid), 1, "{calls:?}");
        }
        assert_tiles_cover(&backend, &[1, 2], WORK_AREA);
        assert_eq!(recorded_work_area(&dir), Some(WORK_AREA));
        assert_eq!(core.layout().windows().len(), 2);
    }

    #[test]
    fn work_area_change_keeps_the_panel_reservation() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        core.startup(backend.windows());
        let panel = Rect::new(0.0, 0.0, 1440.0, 32.0);
        backend.add_window(PANEL, panel);
        backend.set_app_id(PANEL, "cosmic-mac-panel");
        core.platform_event(created(PANEL));
        assert_eq!(core.applier().work_area(), RESERVED);

        // The display changes: 1440×900 with the native menu bar visible.
        let visible = Rect::new(0.0, 25.0, 1440.0, 875.0);
        core.set_work_area(visible);

        let reserved = Rect::new(0.0, 32.0, 1440.0, 868.0);
        assert_eq!(core.applier().work_area(), reserved);
        assert_eq!(core.layout().work_area(), reserved);
        assert_tiles_cover(&backend, &[1, 2], reserved);

        // The same display area again is a no-op.
        let marker = backend.calls().len();
        core.set_work_area(visible);
        let calls = backend.calls();
        assert!(calls[marker..].is_empty(), "{calls:?}");
    }

    #[test]
    fn startup_with_a_running_panel_tiles_below_it() {
        let (mut core, backend, dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (PANEL, PANEL_STRIP),
        ]);
        backend.set_app_id(PANEL, "cosmic-mac-panel");
        backend.set_level(PANEL, 25);

        core.track_shell_windows(backend.shell_windows());
        assert_eq!(core.applier().work_area(), RESERVED);
        let report = core.startup(backend.windows());

        assert_eq!(report.added, 2);
        assert!(core.layout().window_info(PANEL).is_none());
        assert_eq!(set_frames_after(&backend.calls(), 0, PANEL), 0);
        assert_tiles_cover(&backend, &[1, 2], RESERVED);
        assert_eq!(recorded_work_area(&dir), Some(RESERVED));
    }

    #[test]
    fn startup_never_manages_a_panel_still_at_the_normal_level() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (PANEL, Rect::new(100.0, 384.0, 1000.0, 32.0)),
        ]);
        backend.set_app_id(PANEL, "cosmic-mac-panel");

        let report = core.startup(backend.windows());

        assert_eq!(report.order, vec![1]);
        assert!(core.layout().window_info(PANEL).is_none());
        assert_eq!(set_frames_after(&backend.calls(), 0, PANEL), 0);
    }

    #[test]
    fn tracking_a_panel_before_startup_keeps_the_previous_order() {
        // The previous run recorded the reserved area; tracking the panel
        // must not overwrite the session before startup reads its order.
        let session = SessionState {
            work_area: Some(RESERVED),
            tiled_order: vec![2, 3, 1],
        };
        let frames: Vec<(WindowId, Rect)> = (1..=3)
            .map(|wid| (wid, Rect::new(0.0, 0.0, 400.0, 300.0)))
            .collect();
        let (mut core, backend, _dir) = core_with(&frames, WORK_AREA, Some(session));

        core.track_shell_windows(vec![
            ToplevelInfo::new(PANEL, "cosmic-mac-panel", "")
                .with_level(25)
                .with_initial_frame(PANEL_STRIP),
        ]);
        let report = core.startup(backend.windows());

        assert!(!report.tree_rebuilt);
        assert_eq!(report.order, vec![2, 3, 1]);
        assert_tiles_cover(&backend, &[1, 2, 3], RESERVED);
    }

    // ---------------------------------------------------------------------
    // Suspended windows: minimize, Cmd+H, native full screen
    // ---------------------------------------------------------------------

    /// Startup frame of the suspension tests' windows.
    const START: Rect = Rect::new(0.0, 0.0, 400.0, 300.0);
    /// The test display, which a full-screen window covers.
    const DISPLAY: Rect = Rect::new(0.0, 0.0, 1920.0, 1080.0);
    /// Pid of an application that owns more than one test window.
    const SHARED_APP: i32 = 77;

    fn hidden(wid: WindowId) -> PlatformEvent {
        PlatformEvent::WindowHidden { wid: wid as u32 }
    }

    fn shown(wid: WindowId) -> PlatformEvent {
        PlatformEvent::WindowShown { wid: wid as u32 }
    }

    fn resized(wid: WindowId, frame: Rect) -> PlatformEvent {
        PlatformEvent::WindowResized {
            wid: wid as u32,
            frame: platform_rect(frame),
        }
    }

    /// Calls after `marker` that act on `wid`.
    fn calls_for(calls: &[Call], marker: usize, wid: WindowId) -> Vec<Call> {
        calls[marker..]
            .iter()
            .filter(|call| match call {
                Call::SetFrame(w, _)
                | Call::Probe(w)
                | Call::ForgetProbe(w)
                | Call::Focus(w)
                | Call::Park(w, _)
                | Call::Unpark(w) => *w == wid,
                Call::StackBar(_) => false,
            })
            .cloned()
            .collect()
    }

    fn started(wids: &[WindowId]) -> (Core, Arc<TestBackend>, tempfile::TempDir) {
        let frames: Vec<(WindowId, Rect)> = wids.iter().map(|&wid| (wid, START)).collect();
        let (mut core, backend, dir) = core(&frames);
        core.startup(backend.windows());
        (core, backend, dir)
    }

    /// Teach the core a 600 px minimum width for window 1 through a clamp and
    /// a probe (the work area shrinks to 900 px wide).
    fn learn_min_width_of_window_1(core: &mut Core, backend: &TestBackend) -> Rect {
        backend.set_min_size(1, Size::new(600.0, 100.0));
        let area = Rect::new(0.0, 0.0, 900.0, 800.0);
        core.set_work_area(area);
        assert_eq!(
            core.constraints(1).and_then(|learned| learned.min),
            Some(Size::new(600.0, 100.0))
        );
        assert_eq!(backend.probes(), 1);
        area
    }

    #[test]
    fn minimized_window_is_suspended_and_the_rest_reflow() {
        let (mut core, backend, _dir) = started(&[1, 2, 3]);
        assert_eq!(core.layout().focused(), Some(1));
        let marker = backend.calls().len();

        backend.set_minimized(1, true);
        core.platform_event(hidden(1));

        assert!(core.is_suspended(1));
        assert!(core.layout().window_info(1).is_none());
        let calls = backend.calls();
        assert_eq!(calls_for(&calls, marker, 1), [], "{calls:?}");
        assert_tiles_cover(&backend, &[2, 3], WORK_AREA);
        // Focus moves on to a window that is still on screen.
        let focused = core.layout().focused().expect("focus moves on");
        assert_ne!(focused, 1);
        assert!(calls[marker..].contains(&Call::Focus(focused)), "{calls:?}");
        // The original frame stays journaled for shutdown and crash replay.
        assert!(core.applier().journal().contains(1));
        // list-windows reports managed windows only.
        let Response::Windows(windows) = core.request(Request::ListWindows) else {
            panic!("expected a window list");
        };
        let listed: Vec<WindowId> = windows.iter().map(|state| state.info.wid).collect();
        assert_eq!(listed, vec![2, 3]);
    }

    #[test]
    fn a_hidden_notification_for_a_window_that_is_not_minimized_is_ignored() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        let marker = backend.calls().len();

        // SkyLight's window-hidden notification may fire on a Space change.
        core.platform_event(hidden(2));
        core.platform_event(hidden(99));

        assert!(!core.is_suspended(2));
        assert!(core.layout().window_info(2).is_some());
        let calls = backend.calls();
        assert!(calls[marker..].is_empty(), "{calls:?}");
    }

    #[test]
    fn deminimized_window_comes_back_with_its_learned_constraints() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        let area = learn_min_width_of_window_1(&mut core, &backend);
        let learned = core.constraints(1);

        backend.set_minimized(1, true);
        core.platform_event(hidden(1));
        assert!(core.is_suspended(1));
        assert_eq!(core.constraints(1), learned, "suspending keeps them");
        assert_tiles_cover(&backend, &[2], area);

        let marker = backend.calls().len();
        backend.set_minimized(1, false);
        core.platform_event(shown(1));

        assert!(!core.is_suspended(1));
        assert_eq!(
            core.layout().window_info(1).and_then(|info| info.min_size),
            Some(Size::new(600.0, 100.0)),
            "the layout gets the learned minimum back"
        );
        assert_eq!(core.layout().focused(), Some(1));
        assert_eq!(backend.probes(), 1, "no re-probe");
        let calls = backend.calls();
        assert!(set_frames_after(&calls, marker, 1) >= 1, "{calls:?}");
        assert_tiles_cover(&backend, &[1, 2], area);
        assert!(backend.frame(1).unwrap().width >= 600.0);
    }

    fn minimized(wid: WindowId) -> PlatformEvent {
        PlatformEvent::WindowMinimized { wid: wid as u32 }
    }

    fn deminimized(wid: WindowId) -> PlatformEvent {
        PlatformEvent::WindowDeminimized { wid: wid as u32 }
    }

    #[test]
    fn the_ax_minimize_notification_suspends_before_the_attribute_flips() {
        let (mut core, backend, _dir) = started(&[1, 2, 3]);
        let marker = backend.calls().len();

        // `AXMinimized` still reads false when the notification arrives.
        core.platform_event(minimized(1));

        assert!(core.is_suspended(1));
        assert!(core.layout().window_info(1).is_none());
        let calls = backend.calls();
        assert_eq!(calls_for(&calls, marker, 1), [], "{calls:?}");
        assert_tiles_cover(&backend, &[2, 3], WORK_AREA);
        assert_ne!(core.layout().focused(), Some(1));
        assert!(core.applier().journal().contains(1));
        // Unmanaged and unknown windows are ignored.
        core.platform_event(minimized(99));
        assert!(!core.is_suspended(99));
    }

    #[test]
    fn notifications_during_the_minimize_animation_do_not_resume_the_window() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        core.applier.set_debounce(Duration::ZERO);
        let tile = backend.frame(1).unwrap();
        let at = Instant::now();
        core.platform_event_at(minimized(1), at);
        assert!(core.is_suspended(1));

        // The attribute still reads false; the animation moves the window,
        // SkyLight reports it shown or created, focus passes over it.
        let during = at + Duration::from_millis(500);
        core.platform_event_at(shown(1), during);
        core.platform_event_at(resized(1, Rect::new(10.0, 10.0, 100.0, 80.0)), during);
        core.platform_event_at(
            PlatformEvent::FocusChanged {
                wid: Some(1),
                pid: Some(test_pid(1)),
            },
            during,
        );
        assert!(core.is_suspended(1));
        assert_tiles_cover(&backend, &[2], WORK_AREA);

        // Once the attribute has settled a read decides again: still
        // minimized keeps it away ...
        let settled = at + MINIMIZE_SETTLE + Duration::from_millis(100);
        backend.set_minimized(1, true);
        core.platform_event_at(shown(1), settled);
        assert!(core.is_suspended(1));

        // ... and a window that is back although its deminimize notification
        // was missed is resumed by the next trigger.
        backend.set_minimized(1, false);
        backend.set_frame_externally(1, tile);
        core.platform_event_at(shown(1), settled);
        assert!(!core.is_suspended(1));
        assert_tiles_cover(&backend, &[1, 2], WORK_AREA);
    }

    #[test]
    fn the_ax_deminimize_notification_resumes_before_the_attribute_flips() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        let area = learn_min_width_of_window_1(&mut core, &backend);
        backend.set_minimized(1, true);
        core.platform_event(minimized(1));
        assert!(core.is_suspended(1));

        // Restored from the Dock at once: inside the settle time, and
        // `AXMinimized` still reads true.
        let marker = backend.calls().len();
        core.platform_event(deminimized(1));

        assert!(!core.is_suspended(1));
        assert_eq!(
            core.layout().window_info(1).and_then(|info| info.min_size),
            Some(Size::new(600.0, 100.0)),
            "the learned minimum comes back"
        );
        assert_eq!(core.layout().focused(), Some(1));
        assert_eq!(backend.probes(), 1, "no re-probe");
        let calls = backend.calls();
        assert!(set_frames_after(&calls, marker, 1) >= 1, "{calls:?}");
        assert_tiles_cover(&backend, &[1, 2], area);
        // A late duplicate (the SkyLight one) changes nothing.
        core.platform_event(shown(1));
        core.platform_event(deminimized(1));
        assert_tiles_cover(&backend, &[1, 2], area);
    }

    #[test]
    fn the_ax_deminimize_notification_still_checks_the_rest_of_the_window() {
        let frames = [(1, START), (2, START)];
        let (mut core, backend, _dir) = core(&frames);
        backend.set_pid(1, SHARED_APP);
        core.startup(backend.windows());

        // Minimized, then its application hidden: deminimizing alone does not
        // show a hidden application's window.
        core.platform_event(minimized(1));
        core.platform_event(PlatformEvent::AppHidden { pid: SHARED_APP });
        core.platform_event(deminimized(1));
        assert!(core.is_suspended(1));
        core.platform_event(PlatformEvent::AppShown { pid: SHARED_APP });
        assert!(!core.is_suspended(1));
        assert_tiles_cover(&backend, &[1, 2], WORK_AREA);

        // A window that went full screen while minimized stays suspended.
        core.platform_event(minimized(1));
        backend.set_fullscreen(1, true);
        core.platform_event(deminimized(1));
        assert!(core.is_suspended(1));
    }

    #[test]
    fn hiding_an_application_suspends_only_its_windows_until_it_is_shown() {
        let frames = [(1, START), (2, START), (3, START)];
        let (mut core, backend, _dir) = core(&frames);
        // Startup tiles `[[3 / 1] | 2]`: removing 1 first re-flows 3, which
        // is removed next in the same batch.
        backend.set_pid(1, SHARED_APP);
        backend.set_pid(3, SHARED_APP);
        core.startup(backend.windows());
        let marker = backend.calls().len();

        core.platform_event(PlatformEvent::AppHidden { pid: SHARED_APP });

        assert!(core.is_suspended(1));
        assert!(core.is_suspended(3));
        assert!(!core.is_suspended(2));
        let calls = backend.calls();
        assert_eq!(calls_for(&calls, marker, 1), [], "{calls:?}");
        assert_eq!(
            calls_for(&calls, marker, 3),
            [],
            "the re-flow of a window suspended later in the batch is dropped: {calls:?}"
        );
        assert_eq!(
            set_frames_after(&calls, marker, 2),
            1,
            "the whole application is removed in one re-flow: {calls:?}"
        );
        assert_tiles_cover(&backend, &[2], WORK_AREA);
        assert_eq!(core.layout().focused(), Some(2));

        // A window notification alone does not bring a hidden app's window
        // back, and another application being shown changes nothing.
        core.platform_event(shown(1));
        core.platform_event(PlatformEvent::AppShown { pid: test_pid(2) });
        assert!(core.is_suspended(1));
        assert!(core.is_suspended(3));

        core.platform_event(PlatformEvent::AppShown { pid: SHARED_APP });

        assert!(!core.is_suspended(1));
        assert!(!core.is_suspended(3));
        assert_tiles_cover(&backend, &[1, 2, 3], WORK_AREA);
    }

    #[test]
    fn hiding_an_application_leaves_its_parked_windows_where_they_are() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        core.action(Action::Workspace { index: 2 });
        assert!(!core.applier().is_visible(1));
        assert!(backend.saved_frame(1).is_some());
        let marker = backend.calls().len();

        core.platform_event(PlatformEvent::AppHidden { pid: test_pid(1) });

        assert!(core.is_suspended(1));
        let calls = backend.calls();
        assert_eq!(calls_for(&calls, marker, 1), [], "not unparked: {calls:?}");
        // The park record is dropped without moving the window; the journal
        // still restores it from the corner.
        assert_eq!(backend.saved_frame(1), None);
        assert!(core.applier().journal().contains(1));

        // Back on workspace 1 it is not unparked or framed either.
        core.action(Action::Workspace { index: 1 });
        let calls = backend.calls();
        assert_eq!(calls_for(&calls, marker, 1), [], "{calls:?}");
        assert_tiles_cover(&backend, &[2], WORK_AREA);

        // Shown again, it joins the active workspace.
        core.action(Action::Workspace { index: 2 });
        core.platform_event(PlatformEvent::AppShown { pid: test_pid(1) });
        let state = core
            .layout()
            .windows()
            .into_iter()
            .find(|state| state.info.wid == 1)
            .expect("managed again");
        assert_eq!(state.workspace, 2);
        assert_eq!(backend.frame(1), Some(WORK_AREA));
    }

    #[test]
    fn a_minimized_window_of_a_hidden_application_needs_both_back() {
        let frames = [(1, START), (2, START), (3, START)];
        let (mut core, backend, _dir) = core(&frames);
        backend.set_pid(1, SHARED_APP);
        backend.set_pid(2, SHARED_APP);
        core.startup(backend.windows());

        backend.set_minimized(1, true);
        core.platform_event(hidden(1));
        core.platform_event(PlatformEvent::AppHidden { pid: SHARED_APP });
        assert!(core.is_suspended(1));
        assert!(core.is_suspended(2));

        // Shown again with window 1 still in the Dock.
        core.platform_event(PlatformEvent::AppShown { pid: SHARED_APP });
        assert!(core.is_suspended(1));
        assert!(!core.is_suspended(2));

        backend.set_minimized(1, false);
        core.platform_event(shown(1));
        assert!(!core.is_suspended(1));
        assert_tiles_cover(&backend, &[1, 2, 3], WORK_AREA);

        // Minimized and hidden again, then restored from the Dock, which
        // un-hides the application: the window's notification may arrive
        // before the application's, and the window waits for it.
        backend.set_minimized(1, true);
        core.platform_event(hidden(1));
        core.platform_event(PlatformEvent::AppHidden { pid: SHARED_APP });
        backend.set_minimized(1, false);
        core.platform_event(shown(1));
        assert!(core.is_suspended(1));

        core.platform_event(PlatformEvent::AppShown { pid: SHARED_APP });
        assert!(!core.is_suspended(1));
        assert!(!core.is_suspended(2));
        assert_tiles_cover(&backend, &[1, 2, 3], WORK_AREA);
    }

    #[test]
    fn native_full_screen_suspends_the_window_and_leaving_it_resumes() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        core.applier.set_debounce(Duration::ZERO);
        let tile = backend.frame(1).unwrap();
        let marker = backend.calls().len();

        backend.set_fullscreen(1, true);
        backend.set_frame_externally(1, DISPLAY);
        core.platform_event(resized(1, DISPLAY));

        assert!(core.is_suspended(1));
        let calls = backend.calls();
        assert_eq!(
            calls_for(&calls, marker, 1),
            [],
            "no snap-back or invalidation against the transition: {calls:?}"
        );
        assert_tiles_cover(&backend, &[2], WORK_AREA);

        // Further resizes while in full screen keep it suspended.
        core.platform_event(resized(1, DISPLAY));
        assert!(core.is_suspended(1));
        assert_eq!(calls_for(&backend.calls(), marker, 1), []);

        // Leaving full screen resizes it back.
        backend.set_fullscreen(1, false);
        backend.set_frame_externally(1, tile);
        core.platform_event(resized(1, tile));

        assert!(!core.is_suspended(1));
        assert_tiles_cover(&backend, &[1, 2], WORK_AREA);
    }

    #[test]
    fn full_screen_is_only_checked_for_frames_containing_the_work_area() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        core.applier.set_debounce(Duration::ZERO);
        let tile = core.layout().frame_of(1).unwrap();

        // An ordinary drag does not re-read the window.
        let reads = backend.info_reads();
        let dragged = Rect::new(tile.x + 30.0, tile.y + 20.0, tile.width, tile.height);
        backend.set_frame_externally(1, dragged);
        core.platform_event(PlatformEvent::WindowMoved {
            wid: 1,
            frame: platform_rect(dragged),
        });
        assert_eq!(backend.info_reads(), reads);
        assert_eq!(backend.frame(1), Some(tile), "snapped back");

        // A display-sized frame that is not full screen (a zoom) is re-read,
        // then snapped back like any other user resize.
        let marker = backend.calls().len();
        backend.set_frame_externally(1, DISPLAY);
        core.platform_event(resized(1, DISPLAY));
        assert!(backend.info_reads() > reads);
        assert!(!core.is_suspended(1));
        let calls = backend.calls();
        assert!(
            calls[marker..].contains(&Call::SetFrame(1, tile)),
            "{calls:?}"
        );
    }

    #[test]
    fn a_suspended_window_that_takes_focus_is_re_checked() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        core.applier.set_debounce(Duration::ZERO);
        let tile = backend.frame(1).unwrap();
        backend.set_fullscreen(1, true);
        backend.set_frame_externally(1, DISPLAY);
        core.platform_event(resized(1, DISPLAY));
        assert!(core.is_suspended(1));

        // The exit resize is read while AX still reports full screen.
        backend.set_frame_externally(1, tile);
        core.platform_event(resized(1, tile));
        assert!(core.is_suspended(1));

        // Focus arriving afterwards re-checks it.
        backend.set_fullscreen(1, false);
        core.platform_event(PlatformEvent::FocusChanged {
            wid: Some(1),
            pid: Some(test_pid(1)),
        });
        assert!(!core.is_suspended(1));
        assert_eq!(core.layout().focused(), Some(1));
    }

    #[test]
    fn a_window_unreadable_through_ax_stays_suspended() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        backend.set_minimized(1, true);
        core.platform_event(hidden(1));

        // Without AX the minimized flag reads false; that proves nothing.
        backend.set_ax_unreadable(1, true);
        core.platform_event(shown(1));
        assert!(core.is_suspended(1));

        backend.set_ax_unreadable(1, false);
        backend.set_minimized(1, false);
        core.platform_event(shown(1));
        assert!(!core.is_suspended(1));
    }

    #[test]
    fn a_suspended_window_reported_as_created_is_resumed_once() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        backend.set_minimized(1, true);
        core.platform_event(hidden(1));

        // Still minimized: a creation report does not add it.
        core.platform_event(created(1));
        assert!(core.is_suspended(1));
        assert!(core.layout().window_info(1).is_none());

        backend.set_minimized(1, false);
        core.platform_event(created(1));
        core.platform_event(created(1));
        assert!(!core.is_suspended(1));
        assert_eq!(core.layout().windows().len(), 2);
        assert_tiles_cover(&backend, &[1, 2], WORK_AREA);
    }

    #[test]
    fn a_suspended_window_that_closes_is_forgotten() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        learn_min_width_of_window_1(&mut core, &backend);
        backend.set_minimized(1, true);
        core.platform_event(hidden(1));
        assert!(core.applier().journal().contains(1));

        backend.remove_window(1);
        core.platform_event(PlatformEvent::WindowDestroyed { wid: 1 });

        assert!(!core.is_suspended(1));
        assert_eq!(core.constraints(1), None);
        assert!(!core.applier().journal().contains(1));
        let marker = backend.calls().len();
        core.platform_event(shown(1));
        assert!(backend.calls()[marker..].is_empty());
        assert!(core.layout().window_info(1).is_none());
    }

    #[test]
    fn a_suspended_window_is_never_framed_parked_or_probed() {
        let (mut core, backend, _dir) = started(&[1, 2, 3]);
        backend.set_minimized(1, true);
        core.platform_event(hidden(1));
        // Framed, it would clamp to this minimum and be probed.
        backend.set_min_size(1, Size::new(600.0, 100.0));
        let marker = backend.calls().len();

        core.set_work_area(Rect::new(0.0, 0.0, 900.0, 800.0));
        core.action(Action::Workspace { index: 2 });
        core.action(Action::Workspace { index: 1 });
        core.action(Action::Focus {
            direction: cosmic_mac_protocol::Direction::Left,
        });

        let calls = backend.calls();
        assert_eq!(calls_for(&calls, marker, 1), [], "{calls:?}");
        assert_eq!(backend.probes(), 0);
    }

    #[test]
    fn a_clamp_on_a_window_minimized_before_its_notification_suspends_it_unprobed() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        // Minimized, but the notification has not been processed yet.
        backend.set_min_size(1, Size::new(600.0, 100.0));
        backend.set_minimized(1, true);
        let area = Rect::new(0.0, 0.0, 900.0, 800.0);

        core.set_work_area(area);

        assert!(core.is_suspended(1));
        assert_eq!(backend.probes(), 0, "a minimized window is never probed");
        assert_eq!(core.constraints(1), None, "nothing learned from it");
        assert_tiles_cover(&backend, &[2], area);
    }

    #[test]
    fn restore_stops_managing_suspended_windows_too() {
        let (mut core, backend, _dir) = started(&[1, 2]);
        backend.set_minimized(1, true);
        core.platform_event(hidden(1));

        assert_eq!(core.request(Request::Restore), Response::Ok);

        assert!(!core.is_suspended(1));
        backend.set_minimized(1, false);
        core.platform_event(shown(1));
        assert!(core.layout().window_info(1).is_none());
    }
}
