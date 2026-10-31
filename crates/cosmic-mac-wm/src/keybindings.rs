// SPDX-License-Identifier: GPL-3.0-or-later
//! COSMIC-compatible keybinding parsing and the embedded default table.
//!
//! COSMIC stores shortcuts as a RON map from a [`Binding`] to an action name
//! (`com.system76.CosmicSettings.Shortcuts` v1, defaults in cosmic-comp's
//! `data/keybindings.ron`):
//!
//! ```ron
//! {
//!     (modifiers: [Super, Shift], key: "y"): ToggleTiling,
//!     (modifiers: [Super], key: "Left"): Focus(Left),
//!     (modifiers: [Super], key: "1"): Workspace(1),
//! }
//! ```
//!
//! Only the window-management subset is armed in milestone 1.  Launcher, app
//! library, overview and window-switcher actions stay unarmed so that Cmd+Tab
//! and Cmd+Space keep working; `WindowSwitcher` is repurposed as
//! [`Action::FocusNext`] because Super+Tab is the documented milestone-1
//! binding.
//!
//! The `Binding`/`Action` types are vendored here rather than added to
//! `cosmic-mac-protocol`: they are daemon configuration, not part of the IPC
//! contract.

use std::collections::BTreeMap;
use std::path::Path;

use cosmic_mac_platform::HotkeySpec;
use cosmic_mac_protocol::{Action, Direction};
use serde::{Deserialize, Serialize};

/// The embedded milestone-1 defaults.  This is a subset of cosmic-comp's
/// `keybindings.ron` plus the Super+Tab aliases the project specifies.
pub const DEFAULT_KEYBINDINGS: &str = r#"{
    // Focus
    (modifiers: [Super], key: "Left"): Focus(Left),
    (modifiers: [Super], key: "Right"): Focus(Right),
    (modifiers: [Super], key: "Up"): Focus(Up),
    (modifiers: [Super], key: "Down"): Focus(Down),
    (modifiers: [Super], key: "h"): Focus(Left),
    (modifiers: [Super], key: "j"): Focus(Down),
    (modifiers: [Super], key: "k"): Focus(Up),
    (modifiers: [Super], key: "l"): Focus(Right),

    // Move
    (modifiers: [Super, Shift], key: "Left"): Move(Left),
    (modifiers: [Super, Shift], key: "Right"): Move(Right),
    (modifiers: [Super, Shift], key: "Up"): Move(Up),
    (modifiers: [Super, Shift], key: "Down"): Move(Down),
    (modifiers: [Super, Shift], key: "h"): Move(Left),
    (modifiers: [Super, Shift], key: "j"): Move(Down),
    (modifiers: [Super, Shift], key: "k"): Move(Up),
    (modifiers: [Super, Shift], key: "l"): Move(Right),

    // Focus cycling
    (modifiers: [Super], key: "Tab"): FocusNext,
    (modifiers: [Super, Shift], key: "Tab"): FocusPrevious,

    // Window management
    (modifiers: [Super], key: "q"): Close,
    (modifiers: [Super], key: "s"): ToggleStacking,
    (modifiers: [Super], key: "u"): Unstack,
    (modifiers: [Super], key: "y"): ToggleTiling,
    (modifiers: [Super], key: "g"): ToggleWindowFloating,
    (modifiers: [Super], key: "o"): ToggleOrientation,
    (modifiers: [Super], key: "x"): SwapWindow,
    (modifiers: [Super], key: "m"): Maximize,
    (modifiers: [Super], key: "r"): Resizing(Inwards),

    // Workspaces (1-based, matching COSMIC)
    (modifiers: [Super], key: "1"): Workspace(1),
    (modifiers: [Super], key: "2"): Workspace(2),
    (modifiers: [Super], key: "3"): Workspace(3),
    (modifiers: [Super], key: "4"): Workspace(4),
    (modifiers: [Super], key: "5"): Workspace(5),
    (modifiers: [Super], key: "6"): Workspace(6),
    (modifiers: [Super], key: "7"): Workspace(7),
    (modifiers: [Super], key: "8"): Workspace(8),
    (modifiers: [Super], key: "9"): Workspace(9),
    (modifiers: [Super, Shift], key: "1"): MoveToWorkspace(1),
    (modifiers: [Super, Shift], key: "2"): MoveToWorkspace(2),
    (modifiers: [Super, Shift], key: "3"): MoveToWorkspace(3),
    (modifiers: [Super, Shift], key: "4"): MoveToWorkspace(4),
    (modifiers: [Super, Shift], key: "5"): MoveToWorkspace(5),
    (modifiers: [Super, Shift], key: "6"): MoveToWorkspace(6),
    (modifiers: [Super, Shift], key: "7"): MoveToWorkspace(7),
    (modifiers: [Super, Shift], key: "8"): MoveToWorkspace(8),
    (modifiers: [Super, Shift], key: "9"): MoveToWorkspace(9),
}"#;

/// A modifier key in a COSMIC binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Modifier {
    /// Command on macOS (Super in COSMIC).
    Super,
    /// Shift.
    Shift,
    /// Control.
    #[serde(alias = "Control")]
    Ctrl,
    /// Option / Alt.
    Alt,
}

impl Modifier {
    fn order(self) -> u8 {
        match self {
            Modifier::Super => 0,
            Modifier::Shift => 1,
            Modifier::Ctrl => 2,
            Modifier::Alt => 3,
        }
    }

    fn mask(self) -> u64 {
        match self {
            Modifier::Super => HotkeySpec::MASK_COMMAND,
            Modifier::Shift => HotkeySpec::MASK_SHIFT,
            Modifier::Ctrl => HotkeySpec::MASK_CONTROL,
            Modifier::Alt => HotkeySpec::MASK_OPTION,
        }
    }
}

/// A key combination, mirroring COSMIC's `Binding`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Binding {
    /// Modifier list; normalised to Super, Shift, Ctrl, Alt order.
    pub modifiers: Vec<Modifier>,
    /// Key name (`"a"`, `"Left"`, `"Tab"`, ...).
    pub key: String,
}

impl Binding {
    /// Build a binding.
    pub fn new(modifiers: Vec<Modifier>, key: impl Into<String>) -> Self {
        Binding {
            modifiers,
            key: key.into(),
        }
        .normalized()
    }

    /// Normalise modifier order and the key spelling.
    pub fn normalized(mut self) -> Self {
        self.modifiers.sort_by_key(|modifier| modifier.order());
        self.modifiers.dedup();
        if let Some(key) = canonical_key_name(&self.key) {
            self.key = key;
        }
        self
    }

    /// Virtual key code and modifier mask for the macOS event tap.
    pub fn hotkey(&self) -> Option<HotkeySpec> {
        let keycode = keycode_for_name(&self.key)?;
        let modifiers = self
            .modifiers
            .iter()
            .fold(0u64, |mask, modifier| mask | modifier.mask());
        Some(HotkeySpec::new(keycode, modifiers))
    }

    /// Human-readable form used in logs.
    pub fn display(&self) -> String {
        let mut out = String::new();
        for modifier in &self.modifiers {
            out.push_str(match modifier {
                Modifier::Super => "Super+",
                Modifier::Shift => "Shift+",
                Modifier::Ctrl => "Ctrl+",
                Modifier::Alt => "Alt+",
            });
        }
        out.push_str(&self.key);
        out
    }
}

/// A key that the event tap can identify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Hotkey {
    keycode: u16,
    modifiers: u64,
}

/// The parsed keymap.
#[derive(Debug, Clone, Default)]
pub struct KeyMap {
    bindings: BTreeMap<Hotkey, Action>,
    /// Original bindings, kept for diagnostics and tests.
    table: BTreeMap<Binding, Action>,
}

impl KeyMap {
    /// Parse a RON keybinding map.  Unknown keys and unsupported actions are
    /// skipped and reported in the returned warnings.
    pub fn from_ron(text: &str) -> Result<(Self, Vec<String>), String> {
        let mut keymap = KeyMap::default();
        let mut warnings = Vec::new();
        // Parse entry by entry so a single unknown action variant (a newer
        // cosmic-comp action, or a typo) does not discard the whole file.
        for entry in split_entries(text)? {
            let raw: BTreeMap<Binding, RawAction> = match ron::from_str(&format!("{{{entry}}}")) {
                Ok(raw) => raw,
                Err(error) => {
                    warnings.push(format!("skipping keybinding `{entry}`: {error}"));
                    continue;
                }
            };
            for (binding, raw_action) in raw {
                let binding = binding.normalized();
                let Some(action) = raw_action.into_action() else {
                    warnings.push(format!(
                        "{}: action is not armed in milestone 1",
                        binding.display()
                    ));
                    continue;
                };
                let Some(hotkey) = binding.hotkey() else {
                    warnings.push(format!(
                        "{}: key {:?} has no macOS virtual key code",
                        binding.display(),
                        binding.key
                    ));
                    continue;
                };
                keymap.bindings.insert(
                    Hotkey {
                        keycode: hotkey.keycode,
                        modifiers: hotkey.modifiers,
                    },
                    action.clone(),
                );
                keymap.table.insert(binding, action);
            }
        }
        Ok((keymap, warnings))
    }

    /// Load the embedded defaults, then merge `custom.ron` over them when it
    /// exists.  A missing file is not an error; a malformed file is reported
    /// and the defaults are kept.
    pub fn load(custom_path: Option<&Path>) -> (Self, Vec<String>) {
        let (mut keymap, mut warnings) =
            KeyMap::from_ron(DEFAULT_KEYBINDINGS).expect("embedded keybindings must parse");
        let Some(path) = custom_path else {
            return (keymap, warnings);
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (keymap, warnings);
            }
            Err(error) => {
                warnings.push(format!("cannot read {}: {error}", path.display()));
                return (keymap, warnings);
            }
        };
        match KeyMap::from_ron(&text) {
            Ok((custom, custom_warnings)) => {
                warnings.extend(custom_warnings);
                for (binding, action) in custom.table {
                    if let Some(hotkey) = binding.hotkey() {
                        keymap.bindings.insert(
                            Hotkey {
                                keycode: hotkey.keycode,
                                modifiers: hotkey.modifiers,
                            },
                            action.clone(),
                        );
                    }
                    keymap.table.insert(binding, action);
                }
            }
            Err(error) => warnings.push(format!(
                "cannot parse {}: {error}; keeping defaults",
                path.display()
            )),
        }
        (keymap, warnings)
    }

    /// Look up an action for a raw event-tap key event.
    pub fn lookup(&self, keycode: u16, modifiers: u64) -> Option<&Action> {
        self.bindings.get(&Hotkey { keycode, modifiers })
    }

    /// Every binding that the event tap should swallow.
    pub fn hotkeys(&self) -> Vec<HotkeySpec> {
        self.bindings
            .keys()
            .map(|hotkey| HotkeySpec::new(hotkey.keycode, hotkey.modifiers))
            .collect()
    }

    /// The normalised binding table (diagnostics and tests).
    pub fn table(&self) -> &BTreeMap<Binding, Action> {
        &self.table
    }

    /// Number of armed bindings.
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether no binding is armed.
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

/// Split the body of a RON map into top-level `key: value` entries.
///
/// Commas inside bindings, action arguments, strings and comments are ignored.
fn split_entries(text: &str) -> Result<Vec<String>, String> {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(|| "expected a RON map".to_string())?;
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    let mut line_comment = false;
    let mut block_comment = false;
    let mut chars = inner.chars().peekable();
    while let Some(character) = chars.next() {
        if line_comment {
            current.push(character);
            if character == '\n' {
                line_comment = false;
            }
            continue;
        }
        if block_comment {
            current.push(character);
            if character == '*' && chars.peek() == Some(&'/') {
                current.push(chars.next().expect("peeked"));
                block_comment = false;
            }
            continue;
        }
        if in_string {
            current.push(character);
            if escape {
                escape = false;
            } else if character == '\\' {
                escape = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                current.push(character);
            }
            '/' if chars.peek() == Some(&'/') => {
                current.push(character);
                current.push(chars.next().expect("peeked"));
                line_comment = true;
            }
            '/' if chars.peek() == Some(&'*') => {
                current.push(character);
                current.push(chars.next().expect("peeked"));
                block_comment = true;
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(character);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(character);
            }
            ',' if depth == 0 => {
                if !is_blank(&current) {
                    entries.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }
    if in_string || block_comment {
        return Err("unterminated string or block comment".into());
    }
    if depth != 0 {
        return Err("unbalanced brackets in keybinding map".into());
    }
    if !is_blank(&current) {
        entries.push(current.trim().to_string());
    }
    Ok(entries)
}

fn is_blank(text: &str) -> bool {
    text.lines().all(|line| {
        let line = line.trim();
        line.is_empty() || line.starts_with("//")
    })
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
enum RawAction {
    Focus(RawDirection),
    Move(RawDirection),
    Resize(RawDirection),
    SwapWindow,
    ToggleOrientation,
    ToggleStacking,
    ToggleWindowFloating,
    ToggleTiling,
    Unstack,
    Maximize,
    Minimize,
    Fullscreen,
    Close,
    Terminate,
    Debug,
    Workspace(u32),
    MoveToWorkspace(u32),
    LastWorkspace,
    MoveToLastWorkspace,
    NextWorkspace,
    PreviousWorkspace,
    MoveToNextWorkspace,
    MoveToPreviousWorkspace,
    FocusNext,
    FocusPrevious,
    WindowSwitcher,
    WindowSwitcherBackwards,
    Resizing(RawResizeDirection),
    Spawn(String),
}

impl RawAction {
    fn into_action(self) -> Option<Action> {
        Some(match self {
            RawAction::Focus(direction) => Action::Focus {
                direction: direction.into(),
            },
            RawAction::Move(direction) => Action::Move {
                direction: direction.into(),
            },
            RawAction::Resize(direction) => Action::Resize {
                direction: direction.into(),
            },
            RawAction::SwapWindow => Action::SwapWindow,
            RawAction::ToggleOrientation => Action::ToggleOrientation,
            RawAction::ToggleStacking => Action::ToggleStacking,
            RawAction::ToggleWindowFloating => Action::ToggleWindowFloating,
            RawAction::ToggleTiling => Action::ToggleTiling,
            RawAction::Unstack => Action::Unstack,
            RawAction::Maximize => Action::Maximize,
            RawAction::Close => Action::Close,
            RawAction::Workspace(index) => Action::Workspace { index },
            RawAction::MoveToWorkspace(index) => Action::MoveToWorkspace { index },
            RawAction::LastWorkspace => Action::LastWorkspace,
            RawAction::MoveToLastWorkspace => Action::MoveToLastWorkspace,
            RawAction::NextWorkspace => Action::NextWorkspace,
            RawAction::PreviousWorkspace => Action::PreviousWorkspace,
            RawAction::MoveToNextWorkspace => Action::MoveToNextWorkspace,
            RawAction::MoveToPreviousWorkspace => Action::MoveToPreviousWorkspace,
            RawAction::FocusNext | RawAction::WindowSwitcher => Action::FocusNext,
            RawAction::FocusPrevious | RawAction::WindowSwitcherBackwards => Action::FocusPrevious,
            RawAction::Resizing(_) => Action::ResizeMode,
            RawAction::Minimize
            | RawAction::Fullscreen
            | RawAction::Terminate
            | RawAction::Debug
            | RawAction::Spawn(_) => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
enum RawDirection {
    Left,
    Right,
    Up,
    Down,
}

impl From<RawDirection> for Direction {
    fn from(direction: RawDirection) -> Self {
        match direction {
            RawDirection::Left => Direction::Left,
            RawDirection::Right => Direction::Right,
            RawDirection::Up => Direction::Up,
            RawDirection::Down => Direction::Down,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[allow(dead_code)]
enum RawResizeDirection {
    Inwards,
    Outwards,
}

/// Canonical spelling for a key name.  Single characters are lowercased;
/// named keys are matched case-insensitively.
pub fn canonical_key_name(key: &str) -> Option<String> {
    let mut chars = key.chars();
    if let (Some(first), None) = (chars.next(), chars.next())
        && first.is_ascii_alphanumeric()
    {
        return Some(first.to_ascii_lowercase().to_string());
    }
    let canonical = match key.to_ascii_lowercase().as_str() {
        "left" | "arrowleft" => "Left",
        "right" | "arrowright" => "Right",
        "up" | "arrowup" => "Up",
        "down" | "arrowdown" => "Down",
        "tab" => "Tab",
        "return" | "enter" => "Return",
        "escape" | "esc" => "Escape",
        "space" => "Space",
        "backspace" | "delete" => "Backspace",
        "forwarddelete" | "deleteforward" => "ForwardDelete",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" => "PageUp",
        "pagedown" | "page_down" => "PageDown",
        "minus" | "-" => "Minus",
        "equal" | "=" => "Equal",
        "bracketleft" | "[" => "BracketLeft",
        "bracketright" | "]" => "BracketRight",
        "backslash" | "\\" => "Backslash",
        "semicolon" | ";" => "Semicolon",
        "quote" | "'" => "Quote",
        "comma" | "," => "Comma",
        "period" | "." => "Period",
        "slash" | "/" => "Slash",
        "grave" | "`" => "Grave",
        other if other.starts_with('f') && other[1..].parse::<u8>().is_ok() => {
            return Some(other.to_ascii_uppercase());
        }
        _ => return None,
    };
    Some(canonical.to_string())
}

/// macOS virtual key code for a canonical key name (`kVK_*`).
pub fn keycode_for_name(key: &str) -> Option<u16> {
    let key = canonical_key_name(key)?;
    let code = match key.as_str() {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "Equal" => 24,
        "9" => 25,
        "7" => 26,
        "Minus" => 27,
        "8" => 28,
        "0" => 29,
        "BracketRight" => 30,
        "o" => 31,
        "u" => 32,
        "BracketLeft" => 33,
        "i" => 34,
        "p" => 35,
        "Return" => 36,
        "l" => 37,
        "j" => 38,
        "Quote" => 39,
        "k" => 40,
        "Semicolon" => 41,
        "Backslash" => 42,
        "Comma" => 43,
        "Slash" => 44,
        "n" => 45,
        "m" => 46,
        "Period" => 47,
        "Tab" => 48,
        "Space" => 49,
        "Grave" => 50,
        "Backspace" => 51,
        "Escape" => 53,
        "ForwardDelete" => 117,
        "Home" => 115,
        "End" => 119,
        "PageUp" => 116,
        "PageDown" => 121,
        "F1" => 122,
        "F2" => 120,
        "F3" => 99,
        "F4" => 118,
        "F5" => 96,
        "F6" => 97,
        "F7" => 98,
        "F8" => 100,
        "F9" => 101,
        "F10" => 109,
        "F11" => 103,
        "F12" => 111,
        "Left" => 123,
        "Right" => 124,
        "Down" => 125,
        "Up" => 126,
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic_mac_protocol::Direction;

    /// A representative slice of cosmic-comp's real `data/keybindings.ron`,
    /// including actions this daemon does not arm.
    const COSMIC_SAMPLE: &str = r#"{
        (modifiers: [Super], key: "q"): Close,
        (modifiers: [Super], key: "m"): Minimize,
        (modifiers: [Super], key: "Return"): Spawn("cosmic-term"),
        (modifiers: [Super], key: "d"): Spawn("cosmic-app-library"),
        (modifiers: [Super], key: "Left"): Focus(Left),
        (modifiers: [Super], key: "Right"): Focus(Right),
        (modifiers: [Super], key: "Up"): Focus(Up),
        (modifiers: [Super], key: "Down"): Focus(Down),
        (modifiers: [Super], key: "h"): Focus(Left),
        (modifiers: [Super], key: "j"): Focus(Down),
        (modifiers: [Super], key: "k"): Focus(Up),
        (modifiers: [Super], key: "l"): Focus(Right),
        (modifiers: [Super, Shift], key: "Left"): Move(Left),
        (modifiers: [Super, Shift], key: "Right"): Move(Right),
        (modifiers: [Super, Shift], key: "Up"): Move(Up),
        (modifiers: [Super, Shift], key: "Down"): Move(Down),
        (modifiers: [Super], key: "y"): ToggleTiling,
        (modifiers: [Super], key: "g"): ToggleWindowFloating,
        (modifiers: [Super], key: "s"): ToggleStacking,
        (modifiers: [Super], key: "u"): Unstack,
        (modifiers: [Super], key: "o"): ToggleOrientation,
        (modifiers: [Super], key: "x"): SwapWindow,
        (modifiers: [Super], key: "1"): Workspace(1),
        (modifiers: [Super], key: "2"): Workspace(2),
        (modifiers: [Super], key: "3"): Workspace(3),
        (modifiers: [Super], key: "4"): Workspace(4),
        (modifiers: [Super, Shift], key: "1"): MoveToWorkspace(1),
        (modifiers: [Super, Shift], key: "2"): MoveToWorkspace(2),
        (modifiers: [Super, Shift], key: "3"): MoveToWorkspace(3),
        (modifiers: [Super, Shift], key: "4"): MoveToWorkspace(4),
        (modifiers: [Super], key: "Tab"): WindowSwitcher,
        (modifiers: [Super, Shift], key: "Tab"): WindowSwitcherBackwards,
        (modifiers: [Super], key: "r"): Resizing(Outwards),
    }"#;

    fn lookup(keymap: &KeyMap, key: &str, modifiers: &[Modifier]) -> Option<Action> {
        let binding = Binding::new(modifiers.to_vec(), key);
        keymap.table().get(&binding).cloned()
    }

    #[test]
    fn default_table_covers_milestone_one_bindings() {
        let (keymap, warnings) = KeyMap::from_ron(DEFAULT_KEYBINDINGS).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            lookup(&keymap, "Left", &[Modifier::Super]),
            Some(Action::Focus {
                direction: Direction::Left
            })
        );
        assert_eq!(
            lookup(&keymap, "h", &[Modifier::Super]),
            Some(Action::Focus {
                direction: Direction::Left
            })
        );
        assert_eq!(
            lookup(&keymap, "Down", &[Modifier::Super, Modifier::Shift]),
            Some(Action::Move {
                direction: Direction::Down
            })
        );
        assert_eq!(
            lookup(&keymap, "s", &[Modifier::Super]),
            Some(Action::ToggleStacking)
        );
        assert_eq!(
            lookup(&keymap, "y", &[Modifier::Super]),
            Some(Action::ToggleTiling)
        );
        assert_eq!(
            lookup(&keymap, "g", &[Modifier::Super]),
            Some(Action::ToggleWindowFloating)
        );
        assert_eq!(
            lookup(&keymap, "1", &[Modifier::Super]),
            Some(Action::Workspace { index: 1 })
        );
        assert_eq!(
            lookup(&keymap, "4", &[Modifier::Super, Modifier::Shift]),
            Some(Action::MoveToWorkspace { index: 4 })
        );
        assert_eq!(
            lookup(&keymap, "q", &[Modifier::Super]),
            Some(Action::Close)
        );
        assert_eq!(
            lookup(&keymap, "u", &[Modifier::Super]),
            Some(Action::Unstack)
        );
        assert_eq!(
            lookup(&keymap, "Tab", &[Modifier::Super]),
            Some(Action::FocusNext)
        );
        assert_eq!(
            lookup(&keymap, "Tab", &[Modifier::Super, Modifier::Shift]),
            Some(Action::FocusPrevious)
        );
    }

    #[test]
    fn cosmic_sample_parses_and_maps_known_actions() {
        let (keymap, warnings) = KeyMap::from_ron(COSMIC_SAMPLE).unwrap();
        // Spawn/Minimize are deliberately unarmed and reported.
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert_eq!(
            lookup(&keymap, "Left", &[Modifier::Super]),
            Some(Action::Focus {
                direction: Direction::Left
            })
        );
        assert_eq!(
            lookup(&keymap, "1", &[Modifier::Super]),
            Some(Action::Workspace { index: 1 })
        );
        assert_eq!(
            lookup(&keymap, "Tab", &[Modifier::Super]),
            Some(Action::FocusNext)
        );
        assert_eq!(
            lookup(&keymap, "Tab", &[Modifier::Super, Modifier::Shift]),
            Some(Action::FocusPrevious)
        );
        assert_eq!(
            lookup(&keymap, "r", &[Modifier::Super]),
            Some(Action::ResizeMode)
        );
    }

    #[test]
    fn modifier_order_is_normalised() {
        let a = Binding::new(vec![Modifier::Shift, Modifier::Super], "y");
        let b = Binding::new(vec![Modifier::Super, Modifier::Shift], "y");
        assert_eq!(a, b);
        assert_eq!(a.modifiers, vec![Modifier::Super, Modifier::Shift]);
    }

    #[test]
    fn key_names_are_canonicalised() {
        assert_eq!(canonical_key_name("LEFT").as_deref(), Some("Left"));
        assert_eq!(canonical_key_name("enter").as_deref(), Some("Return"));
        assert_eq!(canonical_key_name("A").as_deref(), Some("a"));
        assert_eq!(canonical_key_name("f11").as_deref(), Some("F11"));
        assert_eq!(canonical_key_name("not-a-key"), None);
    }

    #[test]
    fn hotkeys_use_macos_keycodes_and_masks() {
        let binding = Binding::new(vec![Modifier::Super, Modifier::Shift], "y");
        let hotkey = binding.hotkey().unwrap();
        assert_eq!(hotkey.keycode, 16);
        assert_eq!(
            hotkey.modifiers,
            HotkeySpec::MASK_COMMAND | HotkeySpec::MASK_SHIFT
        );
        let (keymap, _) = KeyMap::from_ron(DEFAULT_KEYBINDINGS).unwrap();
        let spec = keymap
            .lookup(16, HotkeySpec::MASK_COMMAND)
            .expect("Super+y is ToggleTiling");
        assert_eq!(*spec, Action::ToggleTiling);
    }

    #[test]
    fn custom_file_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.ron");
        std::fs::write(
            &path,
            r#"{
                (modifiers: [Super], key: "y"): ToggleWindowFloating,
                (modifiers: [Super, Alt], key: "p"): Workspace(7),
            }"#,
        )
        .unwrap();
        let (keymap, warnings) = KeyMap::load(Some(&path));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            lookup(&keymap, "y", &[Modifier::Super]),
            Some(Action::ToggleWindowFloating)
        );
        assert_eq!(
            lookup(&keymap, "p", &[Modifier::Super, Modifier::Alt]),
            Some(Action::Workspace { index: 7 })
        );
        // Unrelated defaults survive the merge.
        assert_eq!(
            lookup(&keymap, "q", &[Modifier::Super]),
            Some(Action::Close)
        );
    }

    #[test]
    fn missing_custom_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (keymap, warnings) = KeyMap::load(Some(&dir.path().join("nope.ron")));
        assert!(warnings.is_empty());
        assert!(keymap.len() > 10);
    }

    #[test]
    fn malformed_custom_file_keeps_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.ron");
        std::fs::write(&path, "{ not valid ron").unwrap();
        let (keymap, warnings) = KeyMap::load(Some(&path));
        assert_eq!(warnings.len(), 1);
        assert!(keymap.len() > 10);
    }

    #[test]
    fn unknown_action_variant_only_skips_that_entry() {
        let (keymap, warnings) = KeyMap::from_ron(
            r#"{
                (modifiers: [Super], key: "q"): Close,
                (modifiers: [Super], key: "z"): SomeFutureAction(Thing),
            }"#,
        )
        .unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            lookup(&keymap, "q", &[Modifier::Super]),
            Some(Action::Close)
        );
    }

    #[test]
    fn unknown_keys_are_reported_not_fatal() {
        let (keymap, warnings) =
            KeyMap::from_ron(r#"{(modifiers: [Super], key: "Hyper"): Close}"#).unwrap();
        assert!(keymap.is_empty());
        assert_eq!(warnings.len(), 1);
    }
}
