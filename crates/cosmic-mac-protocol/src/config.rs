// SPDX-License-Identifier: GPL-3.0-or-later
//! Window-manager configuration (`com.system76.CosmicComp` keys that are meaningful on macOS).

use serde::{Deserialize, Serialize};

use crate::ToplevelInfo;

/// Outer and inner gaps, in logical pixels.
///
/// `outer` is the margin between the work area and the tiling area; `inner` is the space
/// between adjacent tiled windows.  Floating windows are not affected by gaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Gaps {
    pub outer: u32,
    pub inner: u32,
}

/// A rule that makes matching windows float instead of tile.
///
/// `app_id` and `title` are case-insensitive patterns.  A pattern may contain `*` wildcards;
/// without wildcards it matches when it is a substring of the value.  `None` (or an empty
/// pattern, or `"*"`) matches everything.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TilingException {
    pub app_id: Option<String>,
    pub title: Option<String>,
}

impl TilingException {
    pub fn new(app_id: impl Into<String>) -> Self {
        TilingException {
            app_id: Some(app_id.into()),
            title: None,
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// True when this rule applies to `info`.
    pub fn matches(&self, info: &ToplevelInfo) -> bool {
        let app_ok = match &self.app_id {
            Some(pattern) => pattern_matches(pattern, &info.app_id),
            None => true,
        };
        let title_ok = match &self.title {
            Some(pattern) => pattern_matches(pattern, &info.title),
            None => true,
        };
        app_ok && title_ok
    }
}

/// Case-insensitive substring / `*` wildcard match.
fn pattern_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim().to_lowercase();
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    let value = value.to_lowercase();
    if !pattern.contains('*') {
        return value.contains(&pattern);
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0usize;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            if !value.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if index == parts.len() - 1 {
            return value[pos..].ends_with(part);
        } else if let Some(found) = value[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }
    true
}

/// The configuration honoured by the layout engine.
///
/// This is the payload of a config-changed event.  It intentionally only contains keys that
/// affect layout; keybinding and appearance configuration stays in the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub gaps: Gaps,
    /// Active-window hint level.  The layout engine carries the value; the overlay renders it.
    pub active_hint: u32,
    /// Default tiling state for new workspaces.  When false a workspace starts in floating
    /// mode; `ToggleTiling` can still enable tiling for it.
    pub autotile: bool,
    /// Windows matching any of these rules float by default.
    pub tiling_exceptions: Vec<TilingException>,
    /// Application ids whose keybindings must be passed through to the application
    /// (daemon-side; carried here so config-changed is one payload).
    pub passthrough: Vec<String>,
    /// Tapping Super (Command) on its own toggles the launcher, like COSMIC's
    /// `(modifiers: [Super]): System(Launcher)` default binding (daemon-side, like
    /// `passthrough`, which it also honours).  Default on.
    pub super_tap_launcher: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            gaps: Gaps::default(),
            active_hint: 0,
            autotile: true,
            tiling_exceptions: Vec::new(),
            passthrough: Vec::new(),
            super_tap_launcher: true,
        }
    }
}

/// Alias used by IPC/config watchers: the payload of a config-changed event.
pub type ConfigChanged = Config;

impl Config {
    pub fn with_gaps(mut self, outer: u32, inner: u32) -> Self {
        self.gaps = Gaps { outer, inner };
        self
    }

    /// True when a newly opened window must float regardless of the workspace's tiling flag
    /// (dialogs, non-resizable windows and configured exceptions).  Callers also check
    /// `Workspace::tiling_enabled`; `autotile` only controls the initial state of a workspace.
    pub fn should_float(&self, info: &ToplevelInfo) -> bool {
        info.is_dialog
            || !info.resizable
            || self
                .tiling_exceptions
                .iter()
                .any(|exception| exception.matches(info))
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, TilingException};
    use crate::ToplevelInfo;

    fn info(app: &str, title: &str) -> ToplevelInfo {
        ToplevelInfo::new(1, app, title)
    }

    #[test]
    fn config_defaults() {
        let config = Config::default();
        assert_eq!(config.gaps.outer, 0);
        assert_eq!(config.gaps.inner, 0);
        assert_eq!(config.active_hint, 0);
        assert!(config.autotile);
        assert!(config.tiling_exceptions.is_empty());
        assert!(config.passthrough.is_empty());
        assert!(config.super_tap_launcher);
    }

    #[test]
    fn partial_config_deserializes() {
        let config: Config = serde_json::from_str(r#"{"gaps":{"outer":8}}"#).unwrap();
        assert_eq!(config.gaps.outer, 8);
        assert_eq!(config.gaps.inner, 0);
        assert!(config.autotile);
        assert!(config.super_tap_launcher);
    }

    #[test]
    fn super_tap_launcher_can_be_switched_off_in_ron() {
        // The daemon reads `com.system76.CosmicComp/config.ron` as RON.
        let config: Config = ron::from_str("(super_tap_launcher: false)").unwrap();
        assert!(!config.super_tap_launcher);
        assert!(config.autotile);
        let config: Config = ron::from_str("(gaps: (outer: 4))").unwrap();
        assert!(config.super_tap_launcher);
    }

    #[test]
    fn config_round_trip() {
        let config = Config {
            gaps: super::Gaps { outer: 8, inner: 4 },
            active_hint: 2,
            autotile: false,
            tiling_exceptions: vec![TilingException::new("com.apple.finder")],
            passthrough: vec!["com.apple.Terminal".into()],
            super_tap_launcher: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), config);
    }

    #[test]
    fn dialogs_and_non_resizable_float() {
        let config = Config::default();
        assert!(config.should_float(&info("a", "t").with_dialog(true)));
        assert!(config.should_float(&info("a", "t").with_resizable(false)));
        assert!(!config.should_float(&info("a", "t")));
    }

    #[test]
    fn autotile_does_not_override_workspace_rules() {
        // `autotile` selects the initial workspace state; it is not a per-window float rule.
        let config = Config {
            autotile: false,
            ..Config::default()
        };
        assert!(!config.should_float(&info("a", "t")));
    }

    #[test]
    fn exception_substring_matching() {
        let config = Config {
            tiling_exceptions: vec![TilingException::new("picture")],
            ..Config::default()
        };
        assert!(config.should_float(&info("com.apple.PictureInPicture", "video")));
        assert!(!config.should_float(&info("com.apple.TextEdit", "notes")));
    }

    #[test]
    fn exception_wildcard_and_title_matching() {
        let exception = TilingException::new("com.apple.*").with_title("Settings");
        assert!(exception.matches(&info("com.apple.SystemSettings", "Settings")));
        assert!(!exception.matches(&info("com.apple.SystemSettings", "About")));
        assert!(!exception.matches(&info("org.mozilla.firefox", "Settings")));
    }

    #[test]
    fn empty_exception_matches_everything() {
        let exception = TilingException::default();
        assert!(exception.matches(&info("anything", "any title")));
        assert!(TilingException::new("*").matches(&info("anything", "any title")));
    }

    #[test]
    fn case_insensitive_matching() {
        let exception = TilingException::new("SAFARI");
        assert!(exception.matches(&info("com.apple.safari", "x")));
    }
}
