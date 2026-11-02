// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared test harness: a layout plus a fake platform that applies commands.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use cosmic_mac_layout::Layout;
use cosmic_mac_protocol::{
    Action, Command, Config, Rect, STACK_BAR_HEIGHT, Size, StackInfo, ToplevelInfo, TreeNode,
    WindowId,
};

pub const WORK: Rect = Rect::new(0.0, 0.0, 1000.0, 500.0);
pub const TALL_WORK: Rect = Rect::new(0.0, 0.0, 500.0, 1000.0);
pub const BIG_WORK: Rect = Rect::new(0.0, 0.0, 1200.0, 800.0);

pub fn info(wid: WindowId) -> ToplevelInfo {
    ToplevelInfo::new(wid, "com.example.App", format!("Window {wid}"))
}

/// A window with optional `(width, height)` minimum and maximum constraints.
pub fn constrained(
    wid: WindowId,
    min: Option<(f64, f64)>,
    max: Option<(f64, f64)>,
) -> ToplevelInfo {
    let mut info = info(wid);
    if let Some((width, height)) = min {
        info = info.with_min_size(Size::new(width, height));
    }
    if let Some((width, height)) = max {
        info = info.with_max_size(Size::new(width, height));
    }
    info
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
    pub stacks: Vec<StackInfo>,
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
                Command::StacksChanged { stacks } => self.stacks = stacks.clone(),
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

    pub fn update(&mut self, info: ToplevelInfo) -> Vec<Command> {
        let commands = self.layout.update_window(info);
        self.platform.apply(&commands);
        commands
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
            self.platform.stacks,
            self.layout.visible_stacks(),
            "platform tab bars diverged from layout"
        );
    }
}

impl Harness {
    /// Compact rendering of the active tiling tree: `H[1 V[2 3]]` for window 1 beside a
    /// vertical group of 2 over 3 (`H` = side by side, `V` = stacked vertically), `S[1 2]` for
    /// a stack, and an empty string for an empty tree.
    pub fn shape(&self) -> String {
        fn render(node: &TreeNode, out: &mut String) {
            match node {
                TreeNode::Group {
                    orientation,
                    children,
                    ..
                } => {
                    out.push(if orientation.is_horizontal() {
                        'H'
                    } else {
                        'V'
                    });
                    out.push('[');
                    for (index, child) in children.iter().enumerate() {
                        if index > 0 {
                            out.push(' ');
                        }
                        render(child, out);
                    }
                    out.push(']');
                }
                TreeNode::Window { wid, .. } => out.push_str(&wid.to_string()),
                TreeNode::Stack { tabs, .. } => {
                    let wids: Vec<String> = tabs.iter().map(|tab| tab.wid.to_string()).collect();
                    out.push_str(&format!("S[{}]", wids.join(" ")));
                }
            }
        }
        let mut out = String::new();
        if let Some(tree) = self.layout.tree_snapshot() {
            render(&tree, &mut out);
        }
        out
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

pub fn stack_changes(commands: &[Command]) -> Vec<Vec<StackInfo>> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::StacksChanged { stacks } => Some(stacks.clone()),
            _ => None,
        })
        .collect()
}

/// The tabs (window ids) of each stack in a bar update with its focused flag, sorted.
pub fn bar_tabs(stacks: &[StackInfo]) -> Vec<(Vec<WindowId>, bool)> {
    let mut tabs: Vec<_> = stacks
        .iter()
        .map(|stack| {
            (
                stack.tabs.iter().map(|tab| tab.wid).collect(),
                stack.focused,
            )
        })
        .collect();
    tabs.sort();
    tabs
}

/// The stack in `stacks` whose first tab is `wid`.
pub fn stack_with(stacks: &[StackInfo], wid: WindowId) -> &StackInfo {
    stacks
        .iter()
        .find(|stack| stack.tabs.first().is_some_and(|tab| tab.wid == wid))
        .unwrap_or_else(|| panic!("no stack starts with window {wid}: {stacks:?}"))
}

/// Assert that `rects` exactly tile `area` (no overlap, no gaps).
/// The tile every visible window was allocated: the visible frames, with each stack's frame
/// grown back over the tab-bar strip above it.
pub fn visible_tiles(layout: &Layout) -> Vec<Rect> {
    let mut frames = layout.visible_frames();
    for stack in layout.stacks() {
        let wid = stack.tabs[stack.active].wid;
        let frame = frames[&wid];
        assert_eq!(frame, stack.frame, "stack {wid} frame");
        frames.insert(
            wid,
            Rect::new(
                frame.x,
                frame.y - STACK_BAR_HEIGHT,
                frame.width,
                frame.height + STACK_BAR_HEIGHT,
            ),
        );
    }
    frames.into_values().collect()
}

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
