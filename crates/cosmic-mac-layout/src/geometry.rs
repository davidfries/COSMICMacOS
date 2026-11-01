// SPDX-License-Identifier: GPL-3.0-or-later
//! Frame computation for the tiling tree.
//!
//! # Size constraints
//!
//! macOS applications silently clamp or freeze `AXSize` changes, so a layout that ignores the
//! application's minimum and maximum sizes produces flush tiles that the windows do not fill.
//! Every leaf therefore carries an optional [`ToplevelInfo::min_size`] / `max_size` pair.  The
//! geometry walk applies them as follows:
//!
//! 1. Each group allocates its main axis (the axis it splits along) by weight, then clamps each
//!    child to the child's own limits along that axis.  A subtree's limits are aggregated
//!    recursively: a group that splits along the same axis needs the sum of its children's
//!    minima; a group that is perpendicular needs the largest child minimum (and the smallest
//!    child maximum) because its children share that axis.
//! 2. If a child was clamped, the leftover (or deficit) is redistributed among the siblings
//!    that can still grow (or shrink), proportionally to their weights, repeating until the
//!    allocation is stable.  This is the same "clamp and redistribute" idea as flexbox.
//! 3. If **no** sibling can absorb the leftover (every child is at its maximum), the whole
//!    row/column is centred inside the group's rectangle, so the leftover becomes two equal
//!    outer gaps instead of a one-sided strip of wallpaper.
//! 4. A leaf is finally clamped on **both** axes (the parent only constrained its main axis)
//!    and centred in its tile.  Clamping is shrink-only: a minimum larger than the available
//!    tile is ignored rather than allowed to overflow and overlap a sibling.  That keeps the
//!    engine's non-overlap invariant; the platform reports the real result and the daemon
//!    learns the constraint.
//! 5. When even the sum of the minima does not fit, the minima are ignored for that group and
//!    the space is divided by weight.  The application will still clamp its window, but the
//!    layout never emits overlapping rectangles.

use std::collections::{BTreeMap, BTreeSet};

use cosmic_mac_protocol::{
    Gaps, Orientation, Rect, STACK_BAR_HEIGHT, Size, ToplevelInfo, WindowId,
};

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

/// Pixel limits for one window.  `max` uses `f64::INFINITY` for "unbounded".
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowLimits {
    pub min: Size,
    pub max: Size,
}

impl Default for WindowLimits {
    fn default() -> Self {
        WindowLimits {
            min: Size::ZERO,
            max: Size::new(f64::INFINITY, f64::INFINITY),
        }
    }
}

impl WindowLimits {
    /// Convert protocol constraints into internal limits, sanitising invalid values.
    fn from_info(info: &ToplevelInfo) -> Self {
        let min = info.min_size.unwrap_or(Size::ZERO);
        let max = info
            .max_size
            .unwrap_or(Size::new(f64::INFINITY, f64::INFINITY));
        let min = Size::new(min.width.max(0.0), min.height.max(0.0));
        WindowLimits {
            min,
            max: Size::new(
                finite_max(max.width).max(min.width),
                finite_max(max.height).max(min.height),
            ),
        }
    }
}

/// Treat the [`Size::UNBOUNDED`] sentinel (and any non-finite value) as infinity.
fn finite_max(value: f64) -> f64 {
    if value.is_finite() && value < f64::MAX / 2.0 {
        value.max(0.0)
    } else {
        f64::INFINITY
    }
}

/// Limits along one axis.
#[derive(Debug, Clone, Copy, PartialEq)]
struct AxisLimits {
    min: f64,
    max: f64,
}

impl Default for AxisLimits {
    fn default() -> Self {
        AxisLimits {
            min: 0.0,
            max: f64::INFINITY,
        }
    }
}

impl AxisLimits {
    fn new(min: f64, max: f64) -> Self {
        let min = if min.is_finite() { min.max(0.0) } else { 0.0 };
        let max = if max.is_nan() { min } else { max.max(min) };
        AxisLimits { min, max }
    }
}

/// Slack below which an allocation is considered exact, in logical pixels.
const ALLOC_EPSILON: f64 = 1e-6;

/// Lay out `tree` inside `work_area`, applying outer/inner gaps and per-window size limits.
pub(crate) fn compute(
    tree: &Tree,
    work_area: Rect,
    gaps: Gaps,
    infos: &BTreeMap<WindowId, ToplevelInfo>,
) -> Geometry {
    let mut geometry = Geometry::default();
    let area = work_area.inset(gaps.outer as f64);
    if let Some(root) = tree.root {
        walk(tree, root, area, gaps, infos, &mut geometry);
    }
    geometry
}

/// Snap a boundary to a micrometre so repeated fractional weight arithmetic does not leak
/// floating-point noise into frames (e.g., `899.9999999999999` becomes `900.0`).
fn snap(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn axis_extent(rect: Rect, orientation: Orientation) -> f64 {
    if orientation.is_horizontal() {
        rect.width
    } else {
        rect.height
    }
}

fn axis_start(rect: Rect, orientation: Orientation) -> f64 {
    if orientation.is_horizontal() {
        rect.x
    } else {
        rect.y
    }
}

fn limits_for(wid: WindowId, infos: &BTreeMap<WindowId, ToplevelInfo>) -> WindowLimits {
    infos
        .get(&wid)
        .map(WindowLimits::from_info)
        .unwrap_or_default()
}

fn window_axis_limits(
    wid: WindowId,
    axis: Orientation,
    infos: &BTreeMap<WindowId, ToplevelInfo>,
) -> AxisLimits {
    let limits = limits_for(wid, infos);
    if axis.is_horizontal() {
        AxisLimits::new(limits.min.width, limits.max.width)
    } else {
        AxisLimits::new(limits.min.height, limits.max.height)
    }
}

/// Aggregate limits of the subtree at `id` along `axis`.
fn axis_limits(
    tree: &Tree,
    id: NodeId,
    axis: Orientation,
    gaps: Gaps,
    infos: &BTreeMap<WindowId, ToplevelInfo>,
) -> AxisLimits {
    match tree.kind(id) {
        NodeKind::Window(wid) => window_axis_limits(*wid, axis, infos),
        NodeKind::Stack(stack) => stack
            .active_window()
            .map(|wid| window_axis_limits(wid, axis, infos))
            .unwrap_or_default(),
        NodeKind::Group(group) => {
            let same_axis = group.orientation == axis;
            let mut min = 0.0;
            let mut max = if same_axis { 0.0 } else { f64::INFINITY };
            for child in &group.children {
                let child_limits = axis_limits(tree, *child, axis, gaps, infos);
                if same_axis {
                    min += child_limits.min;
                    max += child_limits.max;
                } else {
                    // Children share this axis: the largest minimum and the smallest maximum
                    // decide whether the group itself fits.
                    min = min.max(child_limits.min);
                    max = max.min(child_limits.max);
                }
            }
            if same_axis && group.children.len() > 1 {
                let gap_total = gaps.inner as f64 * (group.children.len() - 1) as f64;
                min += gap_total;
                max += gap_total;
            }
            AxisLimits::new(min, max)
        }
    }
}

/// Clamp a leaf's frame to its own limits on both axes and centre it in its tile.
///
/// Clamping is shrink-only so a minimum larger than the tile cannot overflow into a sibling.
fn clamp_leaf(rect: Rect, limits: WindowLimits) -> Rect {
    let width = rect
        .width
        .clamp(limits.min.width, limits.max.width)
        .min(rect.width)
        .max(0.0);
    let height = rect
        .height
        .clamp(limits.min.height, limits.max.height)
        .min(rect.height)
        .max(0.0);
    if (width - rect.width).abs() <= ALLOC_EPSILON && (height - rect.height).abs() <= ALLOC_EPSILON
    {
        return rect;
    }
    let x = rect.x + (rect.width - width) / 2.0;
    let y = rect.y + (rect.height - height) / 2.0;
    Rect::new(snap(x), snap(y), snap(width), snap(height))
}

/// `rect` without the strip the stack tab bar occupies at its top (never below zero height).
pub(crate) fn below_stack_bar(rect: Rect) -> Rect {
    let bar = STACK_BAR_HEIGHT.min(rect.height.max(0.0));
    Rect::new(rect.x, rect.y + bar, rect.width, rect.height - bar)
}

fn walk(
    tree: &Tree,
    id: NodeId,
    rect: Rect,
    gaps: Gaps,
    infos: &BTreeMap<WindowId, ToplevelInfo>,
    out: &mut Geometry,
) {
    // The node map stores the *tile* a node was allocated, before a leaf is clamped to its
    // application minimum/maximum.  Insertion orientation and resize math want the tile; the
    // actual window frames live in `frames` / `stacks`.
    out.nodes.insert(id, rect);
    match tree.kind(id) {
        NodeKind::Group(group) => {
            let count = group.children.len();
            if count == 0 {
                return;
            }
            let orientation = group.orientation;
            let extent = axis_extent(rect, orientation);
            // Never let gaps consume the whole extent; that would produce inverted rects.
            let gap = if count > 1 {
                (gaps.inner as f64).min(extent / count as f64)
            } else {
                0.0
            };
            let available = (extent - gap * (count - 1) as f64).max(0.0);
            let limits: Vec<AxisLimits> = group
                .children
                .iter()
                .map(|child| axis_limits(tree, *child, orientation, gaps, infos))
                .collect();
            let lengths = allocate(available, &group.sizes, &limits);

            // If every child hit its maximum the row/column cannot fill the group; centre it so
            // the leftover is two symmetric outer gaps instead of wallpaper on one side.
            let content = lengths.iter().sum::<f64>() + gap * (count - 1) as f64;
            let offset = ((extent - content) / 2.0).max(0.0);
            let mut cursor = axis_start(rect, orientation) + offset;
            for (index, child) in group.children.iter().enumerate() {
                let child_start = snap(cursor);
                let child_end = snap(cursor + lengths[index]);
                let length = (child_end - child_start).max(0.0);
                let child_rect = if orientation.is_horizontal() {
                    Rect::new(child_start, rect.y, length, rect.height)
                } else {
                    Rect::new(rect.x, child_start, rect.width, length)
                };
                walk(tree, *child, child_rect, gaps, infos, out);
                cursor += lengths[index] + gap;
            }
        }
        NodeKind::Window(wid) => {
            let frame = clamp_leaf(rect, limits_for(*wid, infos));
            out.frames.insert(*wid, frame);
        }
        NodeKind::Stack(stack) => {
            // The tab bar occupies the top strip of the tile; the members sit below it.
            let below_bar = below_stack_bar(rect);
            let frame = stack
                .active_window()
                .map(|wid| clamp_leaf(below_bar, limits_for(wid, infos)))
                .unwrap_or(below_bar);
            out.stacks.insert(id, frame);
            for (index, wid) in stack.windows.iter().enumerate() {
                if index == stack.active {
                    out.frames.insert(*wid, frame);
                } else {
                    out.hidden.insert(*wid);
                }
            }
        }
    }
}

fn clamp_with_min(value: f64, limits: AxisLimits) -> f64 {
    value.clamp(limits.min, limits.max).max(0.0)
}

fn clamp_without_min(value: f64, limits: AxisLimits) -> f64 {
    value.min(limits.max).max(0.0)
}

/// Allocate `available` pixels among children by weight, honouring per-child limits.
///
/// The result sums to `available` when the constraints allow it.  A smaller sum means every
/// child is at its maximum and the caller must centre the group.  A larger sum never escapes:
/// when the minima do not fit, they are ignored and the space is split by weight so the layout
/// remains non-overlapping.
fn allocate(available: f64, weights: &[f64], limits: &[AxisLimits]) -> Vec<f64> {
    let count = weights.len();
    if count == 0 {
        return Vec::new();
    }
    let available = available.max(0.0);
    let raw_total: f64 = weights.iter().copied().filter(|weight| *weight > 0.0).sum();
    let normalized: Vec<f64> = if raw_total > 0.0 {
        weights.iter().map(|weight| weight.max(0.0)).collect()
    } else {
        vec![1.0; count]
    };
    let total: f64 = normalized.iter().sum();
    let total = if total > 0.0 { total } else { count as f64 };
    let desired: Vec<f64> = normalized
        .iter()
        .map(|weight| available * weight / total)
        .collect();
    let mut lengths = desired.clone();
    for (length, limits) in lengths.iter_mut().zip(limits) {
        *length = clamp_with_min(*length, *limits);
    }

    let mut enforce_min = true;
    for _ in 0..32 {
        let sum: f64 = lengths.iter().sum();
        let diff = available - sum;
        if diff.abs() <= ALLOC_EPSILON {
            break;
        }
        let active: Vec<usize> = (0..count)
            .filter(|&index| {
                if diff > 0.0 {
                    lengths[index] < limits[index].max - ALLOC_EPSILON
                } else {
                    enforce_min && lengths[index] > limits[index].min + ALLOC_EPSILON
                }
            })
            .collect();
        if active.is_empty() {
            if diff < 0.0 && enforce_min {
                // The minima do not fit.  Drop them and split by weight instead of producing
                // overlapping frames.
                enforce_min = false;
                for (index, length) in lengths.iter_mut().enumerate() {
                    *length = clamp_without_min(desired[index], limits[index]);
                }
                continue;
            }
            break;
        }
        let active_weight: f64 = active
            .iter()
            .map(|&index| normalized[index])
            .sum::<f64>()
            .max(ALLOC_EPSILON);
        let mut changed = false;
        for &index in &active {
            let weight = normalized[index].max(ALLOC_EPSILON);
            let share = diff * weight / active_weight;
            let candidate = lengths[index] + share;
            let new = if enforce_min {
                clamp_with_min(candidate, limits[index])
            } else {
                clamp_without_min(candidate, limits[index])
            };
            if (new - lengths[index]).abs() > ALLOC_EPSILON {
                changed = true;
            }
            lengths[index] = new;
        }
        if !changed {
            break;
        }
    }

    // Belt and braces: never emit more than the available extent.
    let sum: f64 = lengths.iter().sum();
    if sum > available + ALLOC_EPSILON && sum > 0.0 {
        let scale = available / sum;
        for length in &mut lengths {
            *length = (*length * scale).max(0.0);
        }
    }
    lengths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(min: f64, max: f64) -> AxisLimits {
        AxisLimits::new(min, max)
    }

    #[test]
    fn allocation_without_constraints_matches_weights() {
        let lengths = allocate(1000.0, &[1.0, 1.0], &[limits(0.0, f64::INFINITY); 2]);
        assert!((lengths[0] - 500.0).abs() < 1e-6);
        assert!((lengths[1] - 500.0).abs() < 1e-6);
    }

    #[test]
    fn allocation_pushes_space_to_the_unclamped_sibling() {
        let lengths = allocate(
            1000.0,
            &[1.0, 1.0],
            &[limits(642.0, f64::INFINITY), limits(0.0, f64::INFINITY)],
        );
        assert!((lengths[0] - 642.0).abs() < 1e-6);
        assert!((lengths[1] - 358.0).abs() < 1e-6);
    }

    #[test]
    fn allocation_leaves_leftover_when_every_child_is_at_max() {
        let lengths = allocate(416.0, &[1.0, 1.0], &[limits(0.0, 100.0); 2]);
        assert!((lengths[0] - 100.0).abs() < 1e-6);
        assert!((lengths[1] - 100.0).abs() < 1e-6);
        assert!((lengths.iter().sum::<f64>() - 200.0).abs() < 1e-6);
    }

    #[test]
    fn allocation_ignores_minima_that_cannot_fit() {
        let lengths = allocate(300.0, &[1.0, 1.0], &[limits(400.0, f64::INFINITY); 2]);
        assert!((lengths[0] - 150.0).abs() < 1e-6);
        assert!((lengths[1] - 150.0).abs() < 1e-6);
    }
}
