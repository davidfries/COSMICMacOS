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

use std::collections::HashMap;
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
    /// Create a core with an initial work area.
    pub fn new(
        config: Config,
        work_area: Rect,
        backend: Arc<dyn PlatformBackend>,
        journal_path: PathBuf,
        session_path: PathBuf,
    ) -> Self {
        let layout = Layout::with_work_area(config, work_area);
        let applier = CommandApplier::new(backend, Journal::new(journal_path), work_area);
        Core {
            layout,
            applier,
            recent_focus: None,
            focus_debounce: Duration::from_millis(250),
            self_pid: std::process::id() as i32,
            session: SessionStore::new(session_path),
            constraints: HashMap::new(),
            constraint_stats: ConstraintStats::default(),
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
            .filter(rules::is_manageable)
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
        self.persist_session();
        StartupReport {
            added,
            tree_rebuilt,
            order,
        }
    }

    /// Feed one platform event.
    pub fn platform_event(&mut self, event: PlatformEvent) {
        match event {
            PlatformEvent::WindowCreated { wid, pid } => self.window_created(wid, pid),
            PlatformEvent::WindowDestroyed { wid } => self.window_destroyed(wid),
            PlatformEvent::WindowMoved { wid, frame }
            | PlatformEvent::WindowResized { wid, frame } => self.window_geometry(wid, frame),
            PlatformEvent::WindowTitleChanged { wid, title } => self.title_changed(wid, title),
            PlatformEvent::WindowOrdered { wid } => self.window_ordered(wid),
            PlatformEvent::FocusChanged { wid, pid } => {
                if let Some(wid) = wid {
                    self.external_focus(u64::from(wid));
                } else if let Some(pid) = pid {
                    self.app_activated(pid);
                }
            }
            PlatformEvent::AppActivated { pid } => {
                if let Some(pid) = pid {
                    self.app_activated(pid);
                }
            }
            PlatformEvent::WindowHidden { .. } | PlatformEvent::WindowShown { .. } => {}
            PlatformEvent::WindowLevelChanged { .. } => {}
            PlatformEvent::DisplayChanged => {}
            PlatformEvent::UnhandledSls { event, wid, len } => {
                log::debug!("unhandled SLS event {event} wid={wid:?} len={len}");
            }
        }
    }

    fn window_created(&mut self, wid: u32, pid: i32) {
        let wid = u64::from(wid);
        if pid == self.self_pid || self.layout.window_info(wid).is_some() {
            return;
        }
        let Some(info) = self.applier.backend().window_info(wid) else {
            log::debug!("window {wid} disappeared before it could be inspected");
            return;
        };
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
        let commands = if self.layout.window_info(wid).is_some() {
            self.layout.window_removed(wid)
        } else {
            Vec::new()
        };
        self.applier.forget_window(wid);
        self.constraints.remove(&wid);
        self.apply(commands);
    }

    fn window_geometry(&mut self, wid: u32, frame: cosmic_mac_platform::Rect) {
        let wid = u64::from(wid);
        let frame = rect_from_platform(frame);
        if self.applier.is_echo(wid, frame) {
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
        let tolerance = crate::backend::AppliedFrame::TOLERANCE;
        if (frame.width - state.frame.width).abs() > tolerance
            || (frame.height - state.frame.height).abs() > tolerance
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

    fn external_focus(&mut self, wid: WindowId) {
        if self.layout.window_info(wid).is_none() {
            return;
        }
        if self.is_recent_focus_event() {
            return;
        }
        log::debug!("external focus -> window {wid}");
        let commands = self.layout.focus_changed(wid);
        self.apply(commands);
    }

    fn app_activated(&mut self, pid: i32) {
        if pid == self.self_pid {
            return;
        }
        if self.is_recent_focus_event() {
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

    /// Display work area changed.  Every window is re-tiled onto the new area
    /// by the layout, and the new area is recorded for the next restart.
    pub fn set_work_area(&mut self, work_area: Rect) {
        if work_area == self.applier.work_area() {
            return;
        }
        log::info!("work area changed to {work_area:?}");
        self.applier.set_work_area(work_area);
        let commands = self.layout.set_work_area(work_area);
        self.apply(commands);
        self.persist_session();
    }

    /// Restore every touched window and drop the journal.
    pub fn shutdown(&mut self) {
        self.persist_session();
        let restored = self.applier.restore_all();
        self.constraints.clear();
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
        if !commands.is_empty() {
            log::debug!("commands: {commands:?}");
        }
        if let Some(wid) = commands.iter().find_map(|command| match command {
            Command::Focus { wid } => Some(*wid),
            _ => None,
        }) {
            self.recent_focus = Some((wid, Instant::now()));
        }
        let clamps = self.applier.apply(&commands);
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
    /// bound.
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
            // Already probed this session: the new observation can tighten the
            // confirmed bounds (for example a Terminal grid snap).
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
    /// authoritative bounds.
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

    fn is_recent_focus_event(&self) -> bool {
        self.recent_focus
            .is_some_and(|(_, at)| at.elapsed() < self.focus_debounce)
    }

    #[cfg(test)]
    fn set_debounce(&mut self, debounce: Duration) {
        self.focus_debounce = debounce;
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
        probe_fails: bool,
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
                    probe_fails: false,
                }),
            }
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
            self.frame(wid).map(|frame| {
                ToplevelInfo::new(wid, "com.example.app", "title").with_initial_frame(frame)
            })
        }
        fn windows(&self) -> Vec<ToplevelInfo> {
            let state = self.state.lock().unwrap();
            state
                .order
                .iter()
                .filter_map(|wid| state.frames.get(wid).copied().map(|frame| (*wid, frame)))
                .map(|(wid, frame)| {
                    ToplevelInfo::new(wid, "com.example.app", "title").with_initial_frame(frame)
                })
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
        core.set_debounce(Duration::ZERO);
        let layout_frame = core.layout().frame_of(1).unwrap();
        core.platform_event(PlatformEvent::WindowMoved {
            wid: 1,
            frame: PlatformRect::new(10.0, 10.0, 50.0, 50.0),
        });
        assert_eq!(backend.frame(1), Some(layout_frame));
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
        assert_eq!(constraints.candidate_min.width, Some(715.0));
        assert!(constraints.candidate_max.width.is_none());

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
        assert_eq!(constraints.candidate_min.width, Some(715.0));
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
        assert_eq!(constraints.candidate_min.width, Some(700.0));
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
}
