// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic pseudo-random action sequences (seeded LCG, no extra dependencies).

mod common;

use common::*;
use cosmic_mac_protocol::{Action, Config, Direction, Rect};

/// Small linear-congruential generator.  Deterministic across runs and platforms.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }
}

fn direction(value: u64) -> Direction {
    match value % 4 {
        0 => Direction::Left,
        1 => Direction::Right,
        2 => Direction::Up,
        _ => Direction::Down,
    }
}

fn check_tiling(harness: &Harness) {
    harness.assert_synced();
    let frames: Vec<Rect> = harness.layout.visible_frames().values().copied().collect();
    if frames.is_empty() {
        assert_eq!(harness.layout.focused(), None);
        return;
    }
    assert_tiles(BIG_WORK, &frames);
    let focused = harness.layout.focused().expect("focus with windows");
    assert!(
        harness.layout.is_visible(focused),
        "focused window {focused} is hidden"
    );
}

#[test]
fn random_tiling_actions_always_cover_the_work_area() {
    let mut harness = Harness::with(Config::default(), BIG_WORK);
    let mut rng = Lcg::new(0xC05_111C);
    let mut wids = Vec::new();
    for _ in 0..3 {
        wids.push(harness.add());
    }

    for step in 0..400 {
        let choice = rng.below(10);
        match choice {
            0 | 1 if wids.len() < 8 => {
                wids.push(harness.add());
            }
            2 if wids.len() > 1 => {
                let index = rng.below(wids.len() as u64) as usize;
                let wid = wids.remove(index);
                harness.remove(wid);
            }
            3 => {
                let dir = direction(rng.next());
                harness.act(Action::Focus { direction: dir });
            }
            4 => {
                let dir = direction(rng.next());
                harness.act(Action::Move { direction: dir });
            }
            5 => {
                let dir = direction(rng.next());
                harness.act(Action::Resize { direction: dir });
            }
            6 => {
                harness.act(Action::ToggleOrientation);
            }
            7 => {
                harness.act(Action::SwapWindow);
            }
            8 => {
                if rng.next().is_multiple_of(2) {
                    harness.act(Action::ToggleStacking);
                } else {
                    harness.act(Action::Unstack);
                }
            }
            _ => {
                if rng.next().is_multiple_of(2) {
                    harness.act(Action::FocusNext);
                } else {
                    harness.act(Action::FocusPrevious);
                }
            }
        }
        check_tiling(&harness);
        assert!(
            wids.len() == harness.layout.windows().len(),
            "window bookkeeping diverged at step {step}"
        );
    }
}

#[test]
fn random_actions_never_produce_overlapping_or_escaping_frames() {
    let config = Config::default().with_gaps(12, 8);
    let mut harness = Harness::with(config, BIG_WORK);
    let mut rng = Lcg::new(0xDEAD_BEEF);
    let mut wids = Vec::new();
    for _ in 0..4 {
        wids.push(harness.add());
    }
    let tiling_area = BIG_WORK.inset(12.0);

    for _ in 0..250 {
        match rng.below(6) {
            0 if wids.len() < 8 => wids.push(harness.add()),
            1 if wids.len() > 1 => {
                let index = rng.below(wids.len() as u64) as usize;
                let wid = wids.remove(index);
                harness.remove(wid);
            }
            2 => {
                harness.act(Action::Focus {
                    direction: direction(rng.next()),
                });
            }
            3 => {
                harness.act(Action::Move {
                    direction: direction(rng.next()),
                });
            }
            4 => {
                harness.act(Action::Resize {
                    direction: direction(rng.next()),
                });
            }
            _ => {
                harness.act(Action::ToggleOrientation);
            }
        }
        harness.assert_synced();
        let frames: Vec<Rect> = harness.layout.visible_frames().values().copied().collect();
        for (index, frame) in frames.iter().enumerate() {
            assert!(
                frame.x >= tiling_area.x - 1e-6
                    && frame.y >= tiling_area.y - 1e-6
                    && frame.right() <= tiling_area.right() + 1e-6
                    && frame.bottom() <= tiling_area.bottom() + 1e-6,
                "frame {frame:?} escapes tiling area {tiling_area:?}"
            );
            assert!(frame.width > 0.0 && frame.height > 0.0);
            for other in &frames[index + 1..] {
                assert!(!frame.overlaps(*other), "{frame:?} overlaps {other:?}");
            }
        }
    }
}

#[test]
fn random_workspace_actions_preserve_every_window() {
    let mut harness = Harness::with(Config::default(), BIG_WORK);
    let mut rng = Lcg::new(0x5EED_1234);
    let mut wids = Vec::new();
    for _ in 0..3 {
        wids.push(harness.add());
    }

    for _ in 0..300 {
        match rng.below(8) {
            0 | 1 if wids.len() < 10 => wids.push(harness.add()),
            2 if wids.len() > 1 => {
                let index = rng.below(wids.len() as u64) as usize;
                let wid = wids.remove(index);
                harness.remove(wid);
            }
            3 => {
                let index = 1 + rng.below(5) as u32;
                harness.act(Action::Workspace { index });
            }
            4 => {
                let index = 1 + rng.below(5) as u32;
                harness.act(Action::MoveToWorkspace { index });
            }
            5 => {
                harness.act(Action::NextWorkspace);
            }
            6 => {
                harness.act(Action::PreviousWorkspace);
            }
            _ => {
                harness.act(Action::LastWorkspace);
            }
        }

        harness.assert_synced();
        let listed = harness.layout.windows();
        assert_eq!(listed.len(), wids.len(), "window lost or duplicated");
        let listed_ids: Vec<u64> = listed.iter().map(|window| window.info.wid).collect();
        let mut expected = wids.clone();
        expected.sort_unstable();
        assert_eq!(listed_ids, expected);

        let workspaces = harness.layout.workspaces();
        assert!(!workspaces.is_empty());
        assert!(harness.layout.active_workspace() as usize <= workspaces.len());
        for (index, workspace) in workspaces.iter().enumerate() {
            assert_eq!(workspace.index, (index + 1) as u32);
            assert_eq!(
                workspace.active,
                index + 1 == harness.layout.active_workspace() as usize
            );
        }
        if !harness.layout.visible_frames().is_empty() {
            let focused = harness
                .layout
                .focused()
                .expect("focus on non-empty workspace");
            assert!(harness.layout.is_visible(focused));
            let focused_state = listed
                .iter()
                .find(|window| window.info.wid == focused)
                .expect("focused window listed");
            assert_eq!(focused_state.workspace, harness.layout.active_workspace());
        }
    }
}

#[test]
fn random_floating_and_tiling_toggles_keep_frames_in_bounds() {
    let mut harness = Harness::with(Config::default(), BIG_WORK);
    let mut rng = Lcg::new(0x00F1_0A77);
    let mut wids = Vec::new();
    for _ in 0..3 {
        wids.push(harness.add());
    }

    for _ in 0..200 {
        match rng.below(6) {
            0 if wids.len() < 8 => wids.push(harness.add()),
            1 if wids.len() > 1 => {
                let index = rng.below(wids.len() as u64) as usize;
                let wid = wids.remove(index);
                harness.remove(wid);
            }
            2 => {
                harness.act(Action::ToggleTiling);
            }
            3 => {
                harness.act(Action::ToggleWindowFloating);
            }
            4 => {
                harness.act(Action::Maximize);
            }
            _ => {
                harness.act(Action::FocusNext);
            }
        }
        harness.assert_synced();
        for frame in harness.layout.visible_frames().values() {
            assert!(
                frame.x >= -1e-6
                    && frame.y >= -1e-6
                    && frame.right() <= BIG_WORK.right() + 1e-6
                    && frame.bottom() <= BIG_WORK.bottom() + 1e-6,
                "frame {frame:?} outside work area"
            );
            assert!(frame.width > 0.0 && frame.height > 0.0);
        }
    }
}
