// SPDX-License-Identifier: GPL-3.0-or-later
//! Frame computation for the tiling tree.

use std::collections::{BTreeMap, BTreeSet};

use cosmic_mac_protocol::{Gaps, Rect, WindowId};

use crate::tree::{NodeId, NodeKind, Tree};

/// The result of laying out one workspace's tiling tree.
#[derive(Debug, Default, Clone)]
pub(crate) struct Geometry {
    /// Frames of visible windows (the active member of each stack).
    pub frames: BTreeMap<WindowId, Rect>,
    /// Windows that are in the tree but do not occupy a frame (inactive stack members).
    pub hidden: BTreeSet<WindowId>,
    /// Frame of every node, including groups and stacks.  Used for resize math and snapshots.
    pub nodes: BTreeMap<NodeId, Rect>,
    /// Frames of stack nodes, keyed by node id.
    pub stacks: BTreeMap<NodeId, Rect>,
}

/// Lay out `tree` inside `work_area`, applying outer and inner gaps.
pub(crate) fn compute(tree: &Tree, work_area: Rect, gaps: Gaps) -> Geometry {
    let mut geometry = Geometry::default();
    let area = work_area.inset(gaps.outer as f64);
    if let Some(root) = tree.root {
        walk(tree, root, area, gaps, &mut geometry);
    }
    geometry
}

/// Snap a boundary to a micrometre so repeated fractional weight arithmetic does not leak
/// floating-point noise into frames (e.g. `899.9999999999999` becomes `900.0`).
fn snap(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn walk(tree: &Tree, id: NodeId, rect: Rect, gaps: Gaps, out: &mut Geometry) {
    out.nodes.insert(id, rect);
    match tree.kind(id) {
        NodeKind::Group(group) => {
            let count = group.children.len();
            if count == 0 {
                return;
            }
            let total: f64 = group.sizes.iter().sum();
            let total = if total > 0.0 { total } else { count as f64 };
            let extent = if group.orientation.is_horizontal() {
                rect.width
            } else {
                rect.height
            };
            // Never let gaps consume the whole extent; that would produce inverted rects.
            let gap = if count > 1 {
                (gaps.inner as f64).min(extent / count as f64)
            } else {
                0.0
            };
            let available = (extent - gap * (count - 1) as f64).max(0.0);
            let start = if group.orientation.is_horizontal() {
                rect.x
            } else {
                rect.y
            };

            let mut cumulative = 0.0;
            for (index, child) in group.children.iter().enumerate() {
                let weight = group.sizes.get(index).copied().unwrap_or(1.0);
                let child_start = start + gap * index as f64 + available * cumulative / total;
                cumulative += weight;
                let child_end = if index + 1 == count {
                    start + extent
                } else {
                    start + gap * index as f64 + available * cumulative / total
                };
                let child_end = snap(child_end.max(child_start));
                let child_start = snap(child_start);
                let length = child_end - child_start;
                let child_rect = if group.orientation.is_horizontal() {
                    Rect::new(child_start, rect.y, length, rect.height)
                } else {
                    Rect::new(rect.x, child_start, rect.width, length)
                };
                walk(tree, *child, child_rect, gaps, out);
            }
        }
        NodeKind::Window(wid) => {
            out.frames.insert(*wid, rect);
        }
        NodeKind::Stack(stack) => {
            out.stacks.insert(id, rect);
            for (index, wid) in stack.windows.iter().enumerate() {
                if index == stack.active {
                    out.frames.insert(*wid, rect);
                } else {
                    out.hidden.insert(*wid);
                }
            }
        }
    }
}
