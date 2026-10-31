// SPDX-License-Identifier: GPL-3.0-or-later
//! The single-threaded layout owner.
//!
//! All mutations happen in [`Core`], which owns the [`Layout`] and the
//! [`CommandApplier`].  The tokio event loop feeds it platform events, key
//! actions and IPC requests; nothing else touches the layout.  Queries are
//! answered from the same state, so the IPC view is always a consistent
//! snapshot.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmic_mac_layout::Layout;
use cosmic_mac_platform::PlatformEvent;
use cosmic_mac_protocol::{Action, Command, Config, Request, Response, ToplevelInfo, WindowId};

use crate::backend::PlatformBackend;
use crate::commands::CommandApplier;
use crate::journal::Journal;
use crate::rules;

/// The daemon's state machine.
pub struct Core {
    layout: Layout,
    applier: CommandApplier,
    recent_focus: Option<(WindowId, Instant)>,
    /// Longer window used to absorb the burst of AppActivated/FocusChanged
    /// events that macOS emits while a programmatic focus change settles.
    focus_debounce: Duration,
    self_pid: i32,
}

impl Core {
    /// Create a core with an initial work area.
    pub fn new(
        config: Config,
        work_area: cosmic_mac_protocol::Rect,
        backend: Arc<dyn PlatformBackend>,
        journal_path: PathBuf,
    ) -> Self {
        let layout = Layout::with_work_area(config, work_area);
        let applier = CommandApplier::new(backend, Journal::new(journal_path), work_area);
        Core {
            layout,
            applier,
            recent_focus: None,
            focus_debounce: Duration::from_millis(250),
            self_pid: std::process::id() as i32,
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

    /// Startup enumeration.  `windows` must be front-to-back (the order of
    /// `CGWindowListCopyWindowInfo`).  Windows are added back-to-front so the
    /// frontmost one is focused last.
    pub fn startup(&mut self, windows: Vec<ToplevelInfo>) -> usize {
        let front = windows
            .iter()
            .find(|info| rules::is_manageable(info))
            .map(|info| info.wid);
        let mut added = 0;
        for info in windows.into_iter().rev() {
            if !rules::is_manageable(&info) {
                continue;
            }
            if self.layout.window_info(info.wid).is_some() {
                continue;
            }
            self.add_window(info);
            added += 1;
        }
        if let Some(front) = front
            && self.layout.window_info(front).is_some()
        {
            self.focus_window(front);
        }
        added
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
            // Floating windows may be moved freely.  The layout keeps its old
            // placement frame; it is only used for diagnostics and snapshots.
            return;
        }
        // A tiled window moved by the user snaps back to its layout slot.
        if let Some(rect) = self.layout.frame_of(wid) {
            if rect.approx_eq(frame, 2.0) {
                return;
            }
            log::debug!("window {wid} moved by user; snapping back to {rect:?}");
            self.applier.reapply_frame(wid, rect);
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
            Request::ListWindows => Response::Windows(self.layout.windows()),
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
                log::info!("restored {restored} windows on request");
                Response::Ok
            }
        }
    }

    /// Display work area changed.
    pub fn set_work_area(&mut self, work_area: cosmic_mac_protocol::Rect) {
        if work_area == self.applier.work_area() {
            return;
        }
        log::info!("work area changed to {work_area:?}");
        self.applier.set_work_area(work_area);
        let commands = self.layout.set_work_area(work_area);
        self.apply(commands);
    }

    /// Restore every touched window and drop the journal.
    pub fn shutdown(&mut self) {
        let restored = self.applier.restore_all();
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
        self.applier.apply(&commands);
        // StackChanged is only emitted when the stack identity changes; a work
        // area change can move the stack without a new command, so re-sync the
        // tab bar after every batch.  The applier de-duplicates.
        if let Err(error) = self.applier.sync_stack_bar(self.layout.active_stack()) {
            log::debug!("tab bar update failed: {error}");
        }
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
    use cosmic_mac_protocol::{Config, Rect, Request, Response, StackInfo, ToplevelInfo};

    use crate::backend::PlatformResult;

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        SetFrame(WindowId, Rect),
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
                }),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.state.lock().unwrap().calls.clone()
        }

        fn frame(&self, wid: WindowId) -> Option<Rect> {
            self.state.lock().unwrap().frames.get(&wid).copied()
        }
    }

    impl PlatformBackend for TestBackend {
        fn set_frame(&self, wid: WindowId, rect: Rect) -> PlatformResult<()> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::SetFrame(wid, rect));
            state.frames.insert(wid, rect);
            Ok(())
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

    fn core(frames: &[(WindowId, Rect)]) -> (Core, Arc<TestBackend>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(TestBackend::new(frames, Rect::new(0.0, 0.0, 1000.0, 800.0)));
        let core = Core::new(
            Config::default(),
            Rect::new(0.0, 0.0, 1000.0, 800.0),
            backend.clone(),
            dir.path().join("frame-journal.ron"),
        );
        (core, backend, dir)
    }

    #[test]
    fn startup_adds_back_to_front_and_focuses_the_front_window() {
        let (mut core, backend, _dir) = core(&[
            (1, Rect::new(0.0, 0.0, 400.0, 300.0)),
            (2, Rect::new(0.0, 0.0, 400.0, 300.0)),
        ]);
        assert_eq!(core.startup(backend.windows()), 2);
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
}
