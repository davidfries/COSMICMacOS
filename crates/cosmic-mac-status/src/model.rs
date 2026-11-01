// SPDX-License-Identifier: GPL-3.0-or-later
//! What the status item shows, as plain data.
//!
//! [`menu`] turns the connection state into a [`MenuModel`]: the menu-bar
//! title, its tooltip and the menu entries with their checkmarks, enabled
//! flags and commands.  The AppKit glue (`crate::appkit`) only copies a model
//! into an `NSStatusItem` and an `NSMenu`, so everything that decides what the
//! user sees is tested here.

use cosmic_mac_protocol::{Action, Request, ShellState, WorkspaceInfo};

/// Title while the window manager is not reachable: a dotted circle, an
/// "empty" glyph that keeps the item visible and findable.
pub const DISCONNECTED_TITLE: &str = "◌";

/// Longest title in characters; longer workspace names are cut with `…`.
pub const MAX_TITLE_CHARS: usize = 12;

/// The menu item shown while the window manager is not reachable.
pub const NOT_RUNNING: &str = "Window manager not running";

/// What the subscription thread last reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// No subscription (not connected yet, or the window manager went away).
    Disconnected,
    /// The latest state the window manager streamed.
    Connected(ShellState),
}

/// What a menu item does when clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCommand {
    /// Switch to the workspace with this 1-based index.
    Workspace(u32),
    /// Toggle tiling for the active workspace.
    ToggleTiling,
    /// Float or tile the focused window.
    ToggleFloating,
    /// Quit the status item (the window manager keeps running).
    Quit,
}

/// `NSMenuItem.tag` values.  0 is AppKit's default (no command) and workspace
/// indices use `1..TAG_TOGGLE_TILING`.
const TAG_TOGGLE_TILING: isize = 1001;
const TAG_TOGGLE_FLOATING: isize = 1002;
const TAG_QUIT: isize = 1003;

impl MenuCommand {
    /// The IPC request for this command; `None` for [`MenuCommand::Quit`],
    /// which the status item handles itself.
    pub fn request(self) -> Option<Request> {
        match self {
            MenuCommand::Workspace(index) => Some(Request::Action(Action::Workspace { index })),
            MenuCommand::ToggleTiling => Some(Request::Action(Action::ToggleTiling)),
            MenuCommand::ToggleFloating => Some(Request::Action(Action::ToggleWindowFloating)),
            MenuCommand::Quit => None,
        }
    }

    /// The `NSMenuItem.tag` that identifies this command.
    pub fn tag(self) -> isize {
        match self {
            MenuCommand::Workspace(index) => index as isize,
            MenuCommand::ToggleTiling => TAG_TOGGLE_TILING,
            MenuCommand::ToggleFloating => TAG_TOGGLE_FLOATING,
            MenuCommand::Quit => TAG_QUIT,
        }
    }

    /// The command a tag stands for, if any.
    pub fn from_tag(tag: isize) -> Option<Self> {
        match tag {
            1..TAG_TOGGLE_TILING => Some(MenuCommand::Workspace(tag as u32)),
            TAG_TOGGLE_TILING => Some(MenuCommand::ToggleTiling),
            TAG_TOGGLE_FLOATING => Some(MenuCommand::ToggleFloating),
            TAG_QUIT => Some(MenuCommand::Quit),
            _ => None,
        }
    }
}

/// One clickable (or disabled) menu item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub title: String,
    /// `None` for an informational item.
    pub command: Option<MenuCommand>,
    pub enabled: bool,
    /// Shown with a checkmark.
    pub checked: bool,
}

impl MenuItem {
    fn action(title: impl Into<String>, command: MenuCommand) -> Self {
        MenuItem {
            title: title.into(),
            command: Some(command),
            enabled: true,
            checked: false,
        }
    }

    fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// A menu entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
}

/// Everything the status item shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    /// The menu-bar title.
    pub title: String,
    /// The menu-bar button's tooltip.
    pub tooltip: String,
    pub entries: Vec<MenuEntry>,
}

/// The status item for `connection`.
pub fn menu(connection: &Connection) -> MenuModel {
    match connection {
        Connection::Disconnected => disconnected(),
        Connection::Connected(state) => connected(state),
    }
}

fn disconnected() -> MenuModel {
    MenuModel {
        title: DISCONNECTED_TITLE.to_string(),
        tooltip: "cosmicMac: window manager not running".to_string(),
        entries: vec![
            MenuEntry::Item(MenuItem {
                title: NOT_RUNNING.to_string(),
                command: None,
                enabled: false,
                checked: false,
            }),
            MenuEntry::Separator,
            quit(),
        ],
    }
}

fn connected(state: &ShellState) -> MenuModel {
    let mut entries: Vec<MenuEntry> = workspace_items(state)
        .into_iter()
        .map(MenuEntry::Item)
        .collect();
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Item(
        MenuItem::action("Tiling", MenuCommand::ToggleTiling).checked(state.tiling),
    ));
    // Floating a window only works while the workspace tiles (with tiling off
    // every window already floats), and needs a focused window.
    let can_float = state.tiling && state.focused_floating.is_some();
    entries.push(MenuEntry::Item(
        MenuItem::action("Float Focused Window", MenuCommand::ToggleFloating)
            .enabled(can_float)
            .checked(can_float && state.focused_floating == Some(true)),
    ));
    entries.push(MenuEntry::Separator);
    entries.push(quit());
    MenuModel {
        title: title(state),
        tooltip: tooltip(state),
        entries,
    }
}

fn quit() -> MenuEntry {
    MenuEntry::Item(MenuItem::action("Quit", MenuCommand::Quit))
}

/// The menu-bar title: the active workspace's name, at most
/// [`MAX_TITLE_CHARS`] characters.
pub fn title(state: &ShellState) -> String {
    match state.active_workspace() {
        Some(workspace) if !workspace.name.trim().is_empty() => {
            truncate(workspace.name.trim(), MAX_TITLE_CHARS)
        }
        Some(workspace) => workspace.index.to_string(),
        None => DISCONNECTED_TITLE.to_string(),
    }
}

/// The menu-bar button's tooltip, e.g. `cosmicMac: workspace 2 of 3, tiling on`.
pub fn tooltip(state: &ShellState) -> String {
    let tiling = if state.tiling { "on" } else { "off" };
    match state.active_workspace() {
        Some(workspace) => format!(
            "cosmicMac: workspace {} of {}, tiling {tiling}",
            workspace.index,
            state.workspaces.len()
        ),
        None => format!("cosmicMac: tiling {tiling}"),
    }
}

/// One item per workspace, plus an empty workspace after the last one.
///
/// The window manager drops empty trailing workspaces once you leave them
/// (COSMIC's dynamic workspaces), so without the extra item the menu could not
/// reach a new workspace.  It is left out when the last workspace is already
/// empty, or the pool is at its maximum.
fn workspace_items(state: &ShellState) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = state
        .workspaces
        .iter()
        .map(|workspace| {
            MenuItem::action(
                workspace_label(workspace),
                MenuCommand::Workspace(workspace.index),
            )
            .checked(workspace.active)
        })
        .collect();
    let last = state.workspaces.last();
    let next = last.map_or(1, |workspace| workspace.index + 1);
    let last_in_use = last.is_none_or(|workspace| workspace.window_count > 0);
    if last_in_use && next <= state.max_workspaces {
        items.push(MenuItem::action(
            workspace_label(&WorkspaceInfo::new(next, next.to_string(), false, 0)),
            MenuCommand::Workspace(next),
        ));
    }
    items
}

/// `Workspace 2`, with the window count when there are windows:
/// `Workspace 2 — 1 window`, `Workspace 1 — 3 windows`.
pub fn workspace_label(workspace: &WorkspaceInfo) -> String {
    let name = if workspace.name.trim().is_empty() {
        workspace.index.to_string()
    } else {
        workspace.name.trim().to_string()
    };
    match workspace.window_count {
        0 => format!("Workspace {name}"),
        1 => format!("Workspace {name} — 1 window"),
        count => format!("Workspace {name} — {count} windows"),
    }
}

/// Cut `text` to `max` characters, ending in `…` when something was cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(workspaces: &[(u32, usize)], active: u32) -> ShellState {
        ShellState {
            workspaces: workspaces
                .iter()
                .map(|&(index, count)| {
                    WorkspaceInfo::new(index, index.to_string(), index == active, count)
                })
                .collect(),
            tiling: true,
            focused_floating: Some(false),
            max_workspaces: 9,
        }
    }

    fn items(model: &MenuModel) -> Vec<&MenuItem> {
        model
            .entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Item(item) => Some(item),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    fn item<'a>(model: &'a MenuModel, title: &str) -> &'a MenuItem {
        items(model)
            .into_iter()
            .find(|item| item.title == title)
            .unwrap_or_else(|| panic!("no item `{title}` in {model:#?}"))
    }

    /// The entries as short strings: `[x]` checked, `( )` disabled, `---` separator.
    fn layout(model: &MenuModel) -> Vec<String> {
        model
            .entries
            .iter()
            .map(|entry| match entry {
                MenuEntry::Separator => "---".to_string(),
                MenuEntry::Item(item) => {
                    let check = if item.checked { "[x] " } else { "" };
                    let disabled = if item.enabled { "" } else { " ( )" };
                    format!("{check}{}{disabled}", item.title)
                }
            })
            .collect()
    }

    #[test]
    fn disconnected_shows_a_neutral_title_and_a_disabled_notice() {
        let model = menu(&Connection::Disconnected);
        assert_eq!(model.title, DISCONNECTED_TITLE);
        assert!(model.tooltip.contains("not running"));
        assert_eq!(
            layout(&model),
            ["Window manager not running ( )", "---", "Quit"]
        );
        let notice = item(&model, NOT_RUNNING);
        assert_eq!(notice.command, None);
        assert_eq!(item(&model, "Quit").command, Some(MenuCommand::Quit));
    }

    #[test]
    fn connected_menu_lists_workspaces_tiling_floating_and_quit() {
        let model = menu(&Connection::Connected(state(&[(1, 3), (2, 1)], 2)));
        assert_eq!(model.title, "2");
        assert_eq!(model.tooltip, "cosmicMac: workspace 2 of 2, tiling on");
        assert_eq!(
            layout(&model),
            [
                "Workspace 1 — 3 windows",
                "[x] Workspace 2 — 1 window",
                "Workspace 3",
                "---",
                "[x] Tiling",
                "Float Focused Window",
                "---",
                "Quit",
            ]
        );
        assert_eq!(
            item(&model, "Workspace 1 — 3 windows").command,
            Some(MenuCommand::Workspace(1))
        );
        assert_eq!(
            item(&model, "Workspace 3").command,
            Some(MenuCommand::Workspace(3))
        );
        assert_eq!(
            item(&model, "Tiling").command,
            Some(MenuCommand::ToggleTiling)
        );
        assert_eq!(
            item(&model, "Float Focused Window").command,
            Some(MenuCommand::ToggleFloating)
        );
    }

    #[test]
    fn exactly_the_active_workspace_is_checked() {
        let model = menu(&Connection::Connected(state(&[(1, 1), (2, 1), (3, 1)], 3)));
        let checked: Vec<_> = items(&model)
            .into_iter()
            .filter(|item| matches!(item.command, Some(MenuCommand::Workspace(_))))
            .filter(|item| item.checked)
            .map(|item| item.command)
            .collect();
        assert_eq!(checked, [Some(MenuCommand::Workspace(3))]);
    }

    #[test]
    fn no_extra_workspace_when_the_last_one_is_empty_or_the_pool_is_full() {
        // The active workspace is empty (just switched there): it is the new one.
        let model = menu(&Connection::Connected(state(&[(1, 2), (2, 0)], 2)));
        assert!(!layout(&model).contains(&"Workspace 3".to_string()));
        assert_eq!(layout(&model)[1], "[x] Workspace 2");

        let full: Vec<(u32, usize)> = (1..=9).map(|index| (index, 1)).collect();
        let model = menu(&Connection::Connected(state(&full, 1)));
        let workspaces = items(&model)
            .into_iter()
            .filter(|item| matches!(item.command, Some(MenuCommand::Workspace(_))))
            .count();
        assert_eq!(workspaces, 9);
    }

    #[test]
    fn tiling_checkmark_follows_the_active_workspace() {
        let mut off = state(&[(1, 1)], 1);
        off.tiling = false;
        let model = menu(&Connection::Connected(off));
        assert!(!item(&model, "Tiling").checked);
        assert!(item(&model, "Tiling").enabled);
        assert!(model.tooltip.ends_with("tiling off"));
    }

    #[test]
    fn float_item_reflects_the_focused_window() {
        let mut floating = state(&[(1, 1)], 1);
        floating.focused_floating = Some(true);
        let model = menu(&Connection::Connected(floating.clone()));
        let float = item(&model, "Float Focused Window");
        assert!(float.enabled && float.checked);

        // Nothing focused: nothing to float.
        floating.focused_floating = None;
        let model = menu(&Connection::Connected(floating.clone()));
        let float = item(&model, "Float Focused Window");
        assert!(!float.enabled && !float.checked);

        // Tiling off: every window floats and the toggle does nothing.
        floating.focused_floating = Some(true);
        floating.tiling = false;
        let model = menu(&Connection::Connected(floating));
        let float = item(&model, "Float Focused Window");
        assert!(!float.enabled && !float.checked);
    }

    #[test]
    fn title_is_the_active_name_truncated_or_neutral_without_one() {
        let mut named = state(&[(1, 0)], 1);
        named.workspaces[0].name = "Mail and calendar".into();
        assert_eq!(title(&named), "Mail and ca…");
        assert_eq!(title(&named).chars().count(), MAX_TITLE_CHARS);
        named.workspaces[0].name = "  ".into();
        assert_eq!(title(&named), "1");
        let none = state(&[(1, 0)], 0);
        assert_eq!(title(&none), DISCONNECTED_TITLE);
        assert_eq!(tooltip(&none), "cosmicMac: tiling on");
    }

    #[test]
    fn an_empty_pool_still_offers_workspace_one() {
        let model = menu(&Connection::Connected(state(&[], 0)));
        assert_eq!(model.title, DISCONNECTED_TITLE);
        assert_eq!(layout(&model)[0], "Workspace 1");
    }

    #[test]
    fn workspace_labels_count_windows() {
        assert_eq!(
            workspace_label(&WorkspaceInfo::new(4, "4", false, 0)),
            "Workspace 4"
        );
        assert_eq!(
            workspace_label(&WorkspaceInfo::new(4, "4", false, 1)),
            "Workspace 4 — 1 window"
        );
        assert_eq!(
            workspace_label(&WorkspaceInfo::new(4, "4", true, 5)),
            "Workspace 4 — 5 windows"
        );
        assert_eq!(
            workspace_label(&WorkspaceInfo::new(4, "", true, 0)),
            "Workspace 4"
        );
    }

    #[test]
    fn commands_map_to_requests() {
        assert_eq!(
            MenuCommand::Workspace(3).request(),
            Some(Request::Action(Action::Workspace { index: 3 }))
        );
        assert_eq!(
            MenuCommand::ToggleTiling.request(),
            Some(Request::Action(Action::ToggleTiling))
        );
        assert_eq!(
            MenuCommand::ToggleFloating.request(),
            Some(Request::Action(Action::ToggleWindowFloating))
        );
        assert_eq!(MenuCommand::Quit.request(), None);
    }

    #[test]
    fn tags_round_trip_and_unknown_tags_are_ignored() {
        for command in [
            MenuCommand::Workspace(1),
            MenuCommand::Workspace(9),
            MenuCommand::ToggleTiling,
            MenuCommand::ToggleFloating,
            MenuCommand::Quit,
        ] {
            assert_ne!(command.tag(), 0, "0 is AppKit's default tag");
            assert_eq!(MenuCommand::from_tag(command.tag()), Some(command));
        }
        for tag in [0, -1, 1004, isize::MAX] {
            assert_eq!(MenuCommand::from_tag(tag), None, "tag {tag}");
        }
    }

    #[test]
    fn every_clickable_item_has_a_distinct_tag() {
        let model = menu(&Connection::Connected(state(&[(1, 1), (2, 1)], 1)));
        let mut tags: Vec<isize> = items(&model)
            .into_iter()
            .filter_map(|item| item.command.map(MenuCommand::tag))
            .collect();
        let count = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), count);
    }
}
