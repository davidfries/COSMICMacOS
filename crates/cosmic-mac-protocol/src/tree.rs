// SPDX-License-Identifier: GPL-3.0-or-later
//! Serializable snapshot of the tiling tree, used by `cosmic-mac-ctl tree`.

use serde::{Deserialize, Serialize};

use crate::{Orientation, Rect, TabInfo, WindowId};

/// One node of the tiling tree.
///
/// Groups are split containers, windows are leaves and stacks are leaves holding an ordered
/// list of windows.  Every node carries the frame it occupies so the CLI can render the tree
/// without recomputing geometry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TreeNode {
    Group {
        orientation: Orientation,
        frame: Rect,
        children: Vec<TreeNode>,
    },
    Window {
        wid: WindowId,
        title: String,
        frame: Rect,
    },
    Stack {
        frame: Rect,
        active: usize,
        tabs: Vec<TabInfo>,
    },
}

impl TreeNode {
    pub fn frame(&self) -> Rect {
        match self {
            TreeNode::Group { frame, .. }
            | TreeNode::Window { frame, .. }
            | TreeNode::Stack { frame, .. } => *frame,
        }
    }

    /// All window ids below this node, in display order.
    pub fn window_ids(&self) -> Vec<WindowId> {
        let mut out = Vec::new();
        self.collect_window_ids(&mut out);
        out
    }

    fn collect_window_ids(&self, out: &mut Vec<WindowId>) {
        match self {
            TreeNode::Group { children, .. } => {
                for child in children {
                    child.collect_window_ids(out);
                }
            }
            TreeNode::Window { wid, .. } => out.push(*wid),
            TreeNode::Stack { tabs, .. } => out.extend(tabs.iter().map(|tab| tab.wid)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TreeNode;
    use crate::{Orientation, Rect, TabInfo};

    #[test]
    fn tree_round_trip_and_window_ids() {
        let tree = TreeNode::Group {
            orientation: Orientation::Horizontal,
            frame: Rect::new(0.0, 0.0, 100.0, 100.0),
            children: vec![
                TreeNode::Window {
                    wid: 1,
                    title: "one".into(),
                    frame: Rect::new(0.0, 0.0, 50.0, 100.0),
                },
                TreeNode::Stack {
                    frame: Rect::new(50.0, 0.0, 50.0, 100.0),
                    active: 0,
                    tabs: vec![
                        TabInfo {
                            wid: 2,
                            title: "two".into(),
                        },
                        TabInfo {
                            wid: 3,
                            title: "three".into(),
                        },
                    ],
                },
            ],
        };
        assert_eq!(tree.window_ids(), vec![1, 2, 3]);
        assert_eq!(tree.frame(), Rect::new(0.0, 0.0, 100.0, 100.0));
        let json = serde_json::to_string(&tree).unwrap();
        assert_eq!(serde_json::from_str::<TreeNode>(&json).unwrap(), tree);
    }

    #[test]
    fn tree_serializes_with_kind_tag() {
        let tree = TreeNode::Window {
            wid: 4,
            title: "x".into(),
            frame: Rect::ZERO,
        };
        let value = serde_json::to_value(&tree).unwrap();
        assert_eq!(value["kind"], "window");
        assert_eq!(value["wid"], 4);
    }
}
