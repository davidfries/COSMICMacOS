// SPDX-License-Identifier: GPL-3.0-or-later
//! Human-readable rendering of protocol payloads.
//!
//! Every renderer returns a string without a trailing newline; `main` prints it.  The
//! `--json` path bypasses this module and prints the protocol payload itself.

use cosmic_mac_protocol::{
    Config, Orientation, Rect, ShellState, TreeNode, WindowState, WorkspaceInfo,
};

/// Truncate to `max` characters, appending `…` when something was cut off.
pub fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn number(value: f64) -> String {
    if (value - value.round()).abs() < 1e-6 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.1}")
    }
}

/// Format a frame as `x,y WxH` in top-left-origin logical pixels.
pub fn format_rect(rect: Rect) -> String {
    format!(
        "{},{} {}x{}",
        number(rect.x),
        number(rect.y),
        number(rect.width),
        number(rect.height)
    )
}

fn cell(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn render_table(columns: &[(&str, bool)], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = columns
        .iter()
        .map(|(header, _)| header.chars().count())
        .collect();
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(value.chars().count());
            }
        }
    }

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(
        columns
            .iter()
            .enumerate()
            .map(|(index, (header, right))| format_cell(header, widths[index], *right))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string(),
    );
    for row in rows {
        lines.push(
            row.iter()
                .enumerate()
                .map(|(index, value)| {
                    let right = columns.get(index).is_some_and(|(_, right)| *right);
                    format_cell(value, widths.get(index).copied().unwrap_or(0), right)
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string(),
        );
    }
    lines.join("\n")
}

fn format_cell(value: &str, width: usize, right: bool) -> String {
    let padding = width.saturating_sub(value.chars().count());
    if right {
        format!("{}{}", " ".repeat(padding), value)
    } else {
        format!("{}{}", value, " ".repeat(padding))
    }
}

/// Render `Response::Windows`.
pub fn render_windows(windows: &[WindowState], limit: Option<usize>) -> String {
    if windows.is_empty() {
        return "no managed windows".to_string();
    }
    let shown = limit.unwrap_or(windows.len()).min(windows.len());
    let columns = [
        ("WID", true),
        ("PID", true),
        ("APP", false),
        ("TITLE", false),
        ("WS", true),
        ("STACK", false),
        ("VIS", false),
        ("FLOAT", false),
        ("FRAME", false),
        ("ACTUAL", false),
    ];
    let rows: Vec<Vec<String>> = windows
        .iter()
        .take(shown)
        .map(|window| {
            let stack = if !window.in_stack {
                "-"
            } else if window.stack_active {
                "active"
            } else {
                "tab"
            };
            vec![
                window.info.wid.to_string(),
                window.info.pid.to_string(),
                truncate(cell(&window.info.app_id), 24),
                truncate(cell(&window.info.title), 32),
                window.workspace.to_string(),
                stack.to_string(),
                yes_no(window.visible).to_string(),
                yes_no(window.floating).to_string(),
                format_rect(window.frame),
                format_actual(window),
            ]
        })
        .collect();
    render_table(&columns, &rows)
}

/// The ACTUAL column: `=` when the real frame matches the layout frame, the real
/// frame when it differs, `?` when it is unknown.
fn format_actual(window: &WindowState) -> String {
    match window.actual_frame {
        Some(actual) if actual.approx_eq(window.frame, 1.0) => "=".to_string(),
        Some(actual) => format_rect(actual),
        None => "?".to_string(),
    }
}

/// Render `Response::Workspaces`.
pub fn render_workspaces(workspaces: &[WorkspaceInfo]) -> String {
    if workspaces.is_empty() {
        return "no workspaces".to_string();
    }
    let columns = [
        ("INDEX", true),
        ("NAME", false),
        ("ACTIVE", false),
        ("WINDOWS", true),
    ];
    let rows: Vec<Vec<String>> = workspaces
        .iter()
        .map(|workspace| {
            vec![
                workspace.index.to_string(),
                cell(&workspace.name).to_string(),
                if workspace.active { "yes" } else { "-" }.to_string(),
                workspace.window_count.to_string(),
            ]
        })
        .collect();
    render_table(&columns, &rows)
}

/// Render one `Response::State` as a single `key=value` line for `watch`, e.g.
/// `workspace=2 workspaces=3 tiling=on focused=tiled windows=1:2,2*:1,3:0`.
pub fn render_state(state: &ShellState) -> String {
    let active = state
        .active_workspace()
        .map(|workspace| workspace.index.to_string())
        .unwrap_or_else(|| "?".to_string());
    let focused = match state.focused_floating {
        None => "none",
        Some(false) => "tiled",
        Some(true) => "floating",
    };
    let windows = state
        .workspaces
        .iter()
        .map(|workspace| {
            let marker = if workspace.active { "*" } else { "" };
            format!("{}{marker}:{}", workspace.index, workspace.window_count)
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "workspace={active} workspaces={} tiling={} focused={focused} windows={}",
        state.workspaces.len(),
        if state.tiling { "on" } else { "off" },
        if windows.is_empty() { "-" } else { &windows },
    )
}

/// Render `Response::Config`.
pub fn render_config(config: &Config) -> String {
    let mut lines = vec![
        format!(
            "gaps: outer={} inner={}",
            config.gaps.outer, config.gaps.inner
        ),
        format!("active_hint: {}", config.active_hint),
        format!("autotile: {}", config.autotile),
    ];

    if config.tiling_exceptions.is_empty() {
        lines.push("tiling_exceptions: (none)".to_string());
    } else {
        lines.push(format!(
            "tiling_exceptions: {} rule(s)",
            config.tiling_exceptions.len()
        ));
        for exception in &config.tiling_exceptions {
            lines.push(format!(
                "  - app_id={} title={}",
                exception.app_id.as_deref().unwrap_or("*"),
                exception.title.as_deref().unwrap_or("*")
            ));
        }
    }

    if config.passthrough.is_empty() {
        lines.push("passthrough: (none)".to_string());
    } else {
        lines.push(format!("passthrough: {} app(s)", config.passthrough.len()));
        for app in &config.passthrough {
            lines.push(format!("  - {app}"));
        }
    }
    lines.push(format!("super_tap_launcher: {}", config.super_tap_launcher));

    lines.join("\n")
}

/// Render `Response::Tree` as an indented tree.
pub fn render_tree(tree: Option<&TreeNode>) -> String {
    let Some(tree) = tree else {
        return "no tiling tree (the active workspace is empty or floating-only)".to_string();
    };
    let mut out = String::new();
    render_node(tree, "", true, true, &mut out);
    out.trim_end().to_string()
}

fn render_node(node: &TreeNode, prefix: &str, is_last: bool, is_root: bool, out: &mut String) {
    let connector = if is_root {
        String::new()
    } else {
        format!("{prefix}{}", if is_last { "└─ " } else { "├─ " })
    };
    let child_prefix = if is_root {
        String::new()
    } else {
        format!("{prefix}{}", if is_last { "   " } else { "│  " })
    };

    match node {
        TreeNode::Group {
            orientation,
            frame,
            children,
        } => {
            let orientation = match orientation {
                Orientation::Horizontal => "horizontal",
                Orientation::Vertical => "vertical",
            };
            out.push_str(&format!(
                "{connector}group {orientation} [{}]\n",
                format_rect(*frame)
            ));
            for (index, child) in children.iter().enumerate() {
                render_node(
                    child,
                    &child_prefix,
                    index + 1 == children.len(),
                    false,
                    out,
                );
            }
        }
        TreeNode::Window { wid, title, frame } => {
            out.push_str(&format!(
                "{connector}window {wid} \"{title}\" [{}]\n",
                format_rect(*frame)
            ));
        }
        TreeNode::Stack {
            frame,
            active,
            tabs,
        } => {
            out.push_str(&format!(
                "{connector}stack [{}] active {}/{}\n",
                format_rect(*frame),
                active + 1,
                tabs.len()
            ));
            for (index, tab) in tabs.iter().enumerate() {
                let marker = if index == *active { "*" } else { " " };
                let last = index + 1 == tabs.len();
                out.push_str(&format!(
                    "{child_prefix}{}{marker} {} \"{}\"\n",
                    if last { "└─" } else { "├─" },
                    tab.wid,
                    tab.title
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_rect, render_config, render_state, render_tree, render_windows, render_workspaces,
        truncate,
    };
    use cosmic_mac_protocol::{
        Config, Gaps, Orientation, Rect, ShellState, TabInfo, TilingException, ToplevelInfo,
        TreeNode, WindowState, WorkspaceInfo,
    };

    #[test]
    fn state_line_lists_the_active_workspace_tiling_focus_and_counts() {
        let state = ShellState {
            workspaces: vec![
                WorkspaceInfo::new(1, "1", false, 2),
                WorkspaceInfo::new(2, "2", true, 1),
                WorkspaceInfo::new(3, "3", false, 0),
            ],
            tiling: true,
            focused_floating: Some(false),
            max_workspaces: 9,
        };
        assert_eq!(
            render_state(&state),
            "workspace=2 workspaces=3 tiling=on focused=tiled windows=1:2,2*:1,3:0"
        );
        let floating = ShellState {
            tiling: false,
            focused_floating: Some(true),
            ..state.clone()
        };
        assert!(render_state(&floating).contains("tiling=off focused=floating"));
        let unfocused = ShellState {
            focused_floating: None,
            ..state
        };
        assert!(render_state(&unfocused).contains("focused=none"));
    }

    #[test]
    fn state_line_without_workspaces() {
        let state = ShellState {
            workspaces: Vec::new(),
            tiling: true,
            focused_floating: None,
            max_workspaces: 9,
        };
        assert_eq!(
            render_state(&state),
            "workspace=? workspaces=0 tiling=on focused=none windows=-"
        );
    }

    fn window(wid: u64, title: &str) -> WindowState {
        WindowState {
            info: ToplevelInfo::new(wid, "com.apple.TextEdit", title).with_pid(42),
            workspace: 1,
            floating: false,
            visible: true,
            focused: false,
            in_stack: false,
            stack_active: false,
            frame: Rect::new(0.0, 0.0, 800.0, 600.0),
            actual_frame: Some(Rect::new(0.0, 0.0, 800.0, 600.0)),
        }
    }

    #[test]
    fn truncate_respects_character_count() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly", 7), "exactly");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abcdef", 1), "…");
        assert_eq!(truncate("abcdef", 0), "");
        assert_eq!(truncate("héllo", 4), "hél…");
    }

    #[test]
    fn rect_formatting_uses_integers_when_possible() {
        assert_eq!(
            format_rect(Rect::new(10.0, 20.0, 800.0, 600.0)),
            "10,20 800x600"
        );
        assert_eq!(
            format_rect(Rect::new(10.5, -0.0, 800.25, 600.0)),
            "10.5,0 800.2x600"
        );
    }

    #[test]
    fn windows_table_has_expected_columns_and_values() {
        let mut focused = window(7, "notes.txt");
        focused.focused = true;
        focused.visible = true;
        let mut stacked = window(8, "tab two");
        stacked.in_stack = true;
        stacked.stack_active = false;
        stacked.visible = false;
        let mut stack_active = window(9, "tab one");
        stack_active.in_stack = true;
        stack_active.stack_active = true;

        let table = render_windows(&[focused, stacked, stack_active], None);
        let header = table.lines().next().unwrap();
        for column in [
            "WID", "PID", "APP", "TITLE", "WS", "STACK", "VIS", "FLOAT", "FRAME", "ACTUAL",
        ] {
            assert!(header.contains(column), "missing {column} in {header}");
        }
        assert!(table.contains("7"));
        assert!(table.contains("42"));
        assert!(table.contains("TextEdit"));
        assert!(table.contains("notes.txt"));
        assert!(table.contains("active"));
        assert!(table.contains("tab"));
        assert!(table.contains("0,0 800x600"));
        assert_eq!(table.lines().count(), 4);
    }

    fn actual_cell(state: WindowState) -> String {
        let table = render_windows(&[state], None);
        let header = table.lines().next().unwrap();
        let start = header.find("ACTUAL").unwrap();
        table.lines().nth(1).unwrap()[start..]
            .trim_end()
            .to_string()
    }

    #[test]
    fn actual_column_shows_equals_when_the_frame_matches() {
        let mut state = window(1, "a");
        // Within the 1 px tolerance.
        state.actual_frame = Some(Rect::new(0.5, -0.5, 800.9, 599.2));
        assert_eq!(actual_cell(state), "=");
    }

    #[test]
    fn actual_column_shows_the_real_frame_when_it_differs() {
        let mut state = window(1, "a");
        state.actual_frame = Some(Rect::new(0.0, 0.0, 726.0, 600.0));
        assert_eq!(actual_cell(state), "0,0 726x600");
    }

    #[test]
    fn actual_column_shows_a_question_mark_when_unknown() {
        let mut state = window(1, "a");
        state.actual_frame = None;
        assert_eq!(actual_cell(state), "?");
    }

    #[test]
    fn windows_table_truncates_long_fields() {
        let long_title = "x".repeat(100);
        let long_app = "a".repeat(100);
        let mut state = window(1, &long_title);
        state.info.app_id = long_app;
        let table = render_windows(&[state], None);
        assert!(table.contains('…'));
        assert!(!table.contains(&long_title));
    }

    #[test]
    fn windows_table_honours_limit_and_empty_input() {
        let windows = vec![window(1, "a"), window(2, "b"), window(3, "c")];
        assert_eq!(render_windows(&windows, Some(2)).lines().count(), 3);
        assert_eq!(render_windows(&windows, Some(0)).lines().count(), 1);
        assert_eq!(render_windows(&[], None), "no managed windows");
    }

    #[test]
    fn workspaces_table_marks_active() {
        let table = render_workspaces(&[
            WorkspaceInfo::new(1, "1", false, 2),
            WorkspaceInfo::new(2, "2", true, 0),
        ]);
        assert!(table.contains("INDEX"));
        assert!(table.contains("yes"));
        let active_line = table.lines().find(|line| line.contains("yes")).unwrap();
        assert!(active_line.contains('2'));
        assert_eq!(render_workspaces(&[]), "no workspaces");
    }

    #[test]
    fn config_rendering_lists_rules() {
        let config = Config {
            gaps: Gaps { outer: 8, inner: 4 },
            active_hint: 2,
            autotile: false,
            tiling_exceptions: vec![
                TilingException::new("com.apple.finder").with_title("Get Info"),
            ],
            passthrough: vec!["com.apple.Terminal".into()],
            super_tap_launcher: false,
        };
        let text = render_config(&config);
        assert!(text.contains("gaps: outer=8 inner=4"));
        assert!(text.contains("active_hint: 2"));
        assert!(text.contains("autotile: false"));
        assert!(text.contains("app_id=com.apple.finder title=Get Info"));
        assert!(text.contains("com.apple.Terminal"));
        assert!(text.contains("super_tap_launcher: false"));
    }

    #[test]
    fn empty_config_rendering() {
        let text = render_config(&Config::default());
        assert!(text.contains("tiling_exceptions: (none)"));
        assert!(text.contains("passthrough: (none)"));
        assert!(text.contains("autotile: true"));
        assert!(text.contains("super_tap_launcher: true"));
    }

    #[test]
    fn tree_rendering_includes_groups_windows_and_stacks() {
        let tree = TreeNode::Group {
            orientation: Orientation::Horizontal,
            frame: Rect::new(0.0, 0.0, 1000.0, 800.0),
            children: vec![
                TreeNode::Window {
                    wid: 1,
                    title: "one".into(),
                    frame: Rect::new(0.0, 0.0, 500.0, 800.0),
                },
                TreeNode::Stack {
                    frame: Rect::new(500.0, 0.0, 500.0, 800.0),
                    active: 1,
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
        let text = render_tree(Some(&tree));
        assert!(text.contains("group horizontal [0,0 1000x800]"));
        assert!(text.contains("window 1 \"one\""));
        assert!(text.contains("stack [500,0 500x800] active 2/2"));
        assert!(text.contains("* 3 \"three\""));
        assert!(text.contains("  2 \"two\""));
        assert_eq!(text.lines().count(), 5);
    }

    #[test]
    fn tree_rendering_handles_empty() {
        assert!(render_tree(None).contains("no tiling tree"));
    }
}
