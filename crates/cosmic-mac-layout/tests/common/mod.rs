// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared test harness: a layout plus a fake platform that applies commands.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use cosmic_mac_layout::Layout;
use cosmic_mac_protocol::{Action, Command, Config, Rect, StackInfo, ToplevelInfo, WindowId};

pub const WORK: Rect = Rect::new(0.0, 0.0, 1000.0, 500.0);
pub const TALL_WORK: Rect = Rect::new(0.0, 0.0, 500.0, 1000.0);
pub const BIG_WORK: Rect = Rect::new(0.0, 0.0, 1200.0, 800.0);

pub fn info(wid: WindowId) -> ToplevelInfo {
    ToplevelInfo::new(wid, "com.example.App", format!("Window {wid}"))
}

pub fn dialog(wid: WindowId) -> ToplevelInfo {
    info(wid)
        .with_dialog(true)
        .with_initial_frame(Rect::new(0.0, 0.0, 400.0, 200.0))
}

#[derive(Debug, Default)]
pub struct FakePlatform {
    pub frames: BTreeMap<WindowId, Rect>,
    pub visible: BTreeSet<WindowId>,
    pub hidden: BTreeSet<WindowId>,
    pub focus: Option<WindowId>,
    pub workspace: Option<u32>,
    pub stack: Option<StackInfo>,
    pub closed: Vec<WindowId>,
}

impl FakePlatform {
    pub fn apply(&mut self, commands: &[Command]) {
        for command in commands {
            match command {
                Command::SetFrame { wid, rect } => {
                    self.frames.insert(*wid, *rect);
                }
                Command::Raise { .. } => {}
                Command::Hide { wid } => {
                    self.visible.remove(wid);
                    self.frames.remove(wid);
                    self.hidden.insert(*wid);
                    if self.focus == Some(*wid) {
                        self.focus = None;
                    }
                }
                Command::Show { wid } => {
                    self.hidden.remove(wid);
                    self.visible.insert(*wid);
                }
                Command::Focus { wid } => self.focus = Some(*wid),
                Command::Unfocus => self.focus = None,
                Command::Close { wid } => self.closed.push(*wid),
                Command::ActivateWorkspace { index } => self.workspace = Some(*index),
                Command::StackChanged { stack } => self.stack = stack.clone(),
            }
        }
    }

    pub fn forget(&mut self, wid: WindowId) {
        self.frames.remove(&wid);
        self.visible.remove(&wid);
        self.hidden.remove(&wid);
        if self.focus == Some(wid) {
            self.focus = None;
        }
    }
}

pub struct Harness {
    pub layout: Layout,
    pub platform: FakePlatform,
    next_wid: WindowId,
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn new() -> Self {
        Self::with(Config::default(), WORK)
    }

    pub fn tall() -> Self {
        Self::with(Config::default(), TALL_WORK)
    }

    pub fn with(config: Config, work_area: Rect) -> Self {
        Harness {
            layout: Layout::with_work_area(config, work_area),
            platform: FakePlatform {
                workspace: Some(1),
                ..FakePlatform::default()
            },
            next_wid: 1,
        }
    }

    pub fn add(&mut self) -> WindowId {
        let wid = self.next_wid;
        self.next_wid += 1;
        self.add_with(info(wid))
    }

    pub fn next_wid(&self) -> WindowId {
        self.next_wid
    }

    pub fn add_with(&mut self, info: ToplevelInfo) -> WindowId {
        let wid = info.wid;
        if wid >= self.next_wid {
            self.next_wid = wid + 1;
        }
        let commands = self.layout.window_added(info);
        self.platform.apply(&commands);
        wid
    }

    pub fn remove(&mut self, wid: WindowId) {
        let commands = self.layout.window_removed(wid);
        self.platform.apply(&commands);
        self.platform.forget(wid);
    }

    pub fn act(&mut self, action: Action) -> Vec<Command> {
        let commands = self.layout.action(action);
        self.platform.apply(&commands);
        commands
    }

    pub fn set_work_area(&mut self, work_area: Rect) -> Vec<Command> {
        let commands = self.layout.set_work_area(work_area);
        self.platform.apply(&commands);
        commands
    }

    pub fn update_config(&mut self, config: Config) -> Vec<Command> {
        let commands = self.layout.update_config(config);
        self.platform.apply(&commands);
        commands
    }

    pub fn frame(&self, wid: WindowId) -> Rect {
        self.layout
            .frame_of(wid)
            .unwrap_or_else(|| panic!("window {wid} has no frame"))
    }

    pub fn assert_frame(&self, wid: WindowId, x: f64, y: f64, width: f64, height: f64) {
        assert_eq!(
            self.layout.frame_of(wid),
            Some(Rect::new(x, y, width, height)),
            "frame of window {wid}"
        );
    }

    /// The fake platform must agree with the layout after every event.
    pub fn assert_synced(&self) {
        assert_eq!(
            self.platform.frames,
            self.layout.visible_frames(),
            "platform frames diverged from layout"
        );
        assert_eq!(
            self.platform.visible,
            self.layout.visible_frames().keys().copied().collect(),
            "platform visibility diverged from layout"
        );
        assert_eq!(
            self.platform.focus,
            self.layout.focused(),
            "platform focus diverged from layout"
        );
        assert_eq!(
            self.platform.workspace,
            Some(self.layout.active_workspace()),
            "platform workspace diverged from layout"
        );
        assert_eq!(
            self.platform.stack,
            self.layout.active_stack(),
            "platform stack diverged from layout"
        );
    }
}

/// Frames from a command batch, in command order (last write wins).
pub fn set_frames(commands: &[Command]) -> BTreeMap<WindowId, Rect> {
    let mut frames = BTreeMap::new();
    for command in commands {
        if let Command::SetFrame { wid, rect } = command {
            frames.insert(*wid, *rect);
        }
    }
    frames
}

pub fn hides(commands: &[Command]) -> Vec<WindowId> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::Hide { wid } => Some(*wid),
            _ => None,
        })
        .collect()
}

pub fn shows(commands: &[Command]) -> Vec<WindowId> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::Show { wid } => Some(*wid),
            _ => None,
        })
        .collect()
}

pub fn focus_command(commands: &[Command]) -> Option<WindowId> {
    commands.iter().find_map(|command| match command {
        Command::Focus { wid } => Some(*wid),
        _ => None,
    })
}

pub fn closes(commands: &[Command]) -> Vec<WindowId> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::Close { wid } => Some(*wid),
            _ => None,
        })
        .collect()
}

pub fn has_unfocus(commands: &[Command]) -> bool {
    commands
        .iter()
        .any(|command| matches!(command, Command::Unfocus))
}

pub fn workspace_activations(commands: &[Command]) -> Vec<u32> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::ActivateWorkspace { index } => Some(*index),
            _ => None,
        })
        .collect()
}

pub fn stack_changes(commands: &[Command]) -> Vec<Option<StackInfo>> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::StackChanged { stack } => Some(stack.clone()),
            _ => None,
        })
        .collect()
}

/// Assert that `rects` exactly tile `area` (no overlap, no gaps).
pub fn assert_tiles(area: Rect, rects: &[Rect]) {
    let mut sum = 0.0;
    for (index, rect) in rects.iter().enumerate() {
        assert!(
            rect.x >= area.x - 1e-6
                && rect.y >= area.y - 1e-3
                && rect.right() <= area.right() + 1e-3
                && rect.bottom() <= area.bottom() + 1e-3,
            "rect {rect:?} escapes area {area:?}"
        );
        assert!(rect.width > 0.0 && rect.height > 0.0, "empty rect {rect:?}");
        sum += rect.area();
        for other in &rects[index + 1..] {
            assert!(!rect.overlaps(*other), "rect {rect:?} overlaps {other:?}");
        }
    }
    assert!(
        (sum - area.area()).abs() < 1e-3,
        "area sum {sum} does not cover {area:?} (expected {})",
        area.area()
    );
}
