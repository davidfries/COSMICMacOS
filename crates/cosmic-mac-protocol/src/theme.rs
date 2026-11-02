// SPDX-License-Identifier: GPL-3.0-or-later
//! Dark/light resolution and the tab bar's palette, shared by the window manager (tab bar) and
//! the launcher.
//!
//! # Mode
//!
//! COSMIC keeps its dark/light choice in the cosmic-config entry `com.system76.CosmicTheme.Mode`,
//! version 1: under the config root ([`config_root`], `~/Library/Application Support/cosmic`,
//! which is cosmic-config's `dirs::config_dir()/cosmic` on macOS) the directory
//! `com.system76.CosmicTheme.Mode/v1/` holds one file per key, each a RON value: `is_dark` and
//! `auto_switch`, both `true` or `false`.
//!
//! [`resolve_mode`] applies one rule: COSMIC's `is_dark` decides when it is readable and
//! `auto_switch` is off; otherwise (no config, an unreadable `is_dark`, or `auto_switch: true`)
//! the macOS appearance decides. A missing or unreadable `auto_switch` counts as `false`,
//! cosmic-config's default. On Linux `auto_switch` means "switch at sunrise and sunset", which
//! cosmic-settings-daemon does by rewriting `is_dark`; on macOS the system appearance (which may
//! itself be "Auto") takes that role.
//!
//! The macOS side is the global default `AppleInterfaceStyle` ([`MACOS_STYLE_KEY`]), `Dark` in
//! dark mode and absent in light mode ([`macos_style_is_dark`]). Each process reads it its own
//! way and passes the result in; macOS posts the distributed notification
//! [`MACOS_THEME_CHANGED_NOTIFICATION`] when it changes.
//!
//! # Palette
//!
//! [`TabBarPalette`] holds the tab bar's colours for each mode, taken from COSMIC's default dark
//! and light themes (`cosmic_theme::Theme::dark_default()` / `light_default()` at the pinned
//! libcosmic revision; a launcher test checks them against libcosmic). The user's accent colour
//! from `com.system76.CosmicTheme.Dark/v1/accent` (or `.Light`) replaces the default accent when
//! that file is readable ([`read_accent`]).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// cosmic-config id of COSMIC's dark/light setting.
pub const MODE_CONFIG_ID: &str = "com.system76.CosmicTheme.Mode";
/// cosmic-config id of COSMIC's dark theme.
pub const DARK_THEME_ID: &str = "com.system76.CosmicTheme.Dark";
/// cosmic-config id of COSMIC's light theme.
pub const LIGHT_THEME_ID: &str = "com.system76.CosmicTheme.Light";
/// Version of the three entries above.
pub const CONFIG_VERSION: u64 = 1;
/// The cosmic-config root relative to the home directory.
pub const CONFIG_ROOT_RELATIVE_PATH: &str = "Library/Application Support/cosmic";

/// Global macOS default holding the appearance: `Dark`, or absent for light.
pub const MACOS_STYLE_KEY: &str = "AppleInterfaceStyle";
/// Distributed notification macOS posts when the appearance changes.
pub const MACOS_THEME_CHANGED_NOTIFICATION: &str = "AppleInterfaceThemeChangedNotification";

/// The cosmic-config root for a home directory.
pub fn config_root(home: &Path) -> PathBuf {
    home.join(CONFIG_ROOT_RELATIVE_PATH)
}

/// The directory of a cosmic-config entry (`<root>/<id>/v1`).
pub fn entry_dir(root: &Path, id: &str) -> PathBuf {
    root.join(id).join(format!("v{CONFIG_VERSION}"))
}

/// Dark or light.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    /// `Dark` for `true`.
    pub const fn from_dark(is_dark: bool) -> Self {
        if is_dark { Mode::Dark } else { Mode::Light }
    }

    pub const fn is_dark(self) -> bool {
        matches!(self, Mode::Dark)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Dark => "dark",
            Mode::Light => "light",
        }
    }

    /// cosmic-config id of COSMIC's theme for this mode.
    pub const fn theme_id(self) -> &'static str {
        match self {
            Mode::Dark => DARK_THEME_ID,
            Mode::Light => LIGHT_THEME_ID,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// COSMIC's dark/light config as read from disk; `None` is a missing or unreadable key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModeConfig {
    pub is_dark: Option<bool>,
    pub auto_switch: Option<bool>,
}

impl ModeConfig {
    /// Reads `com.system76.CosmicTheme.Mode/v1/{is_dark,auto_switch}` under the config root.
    /// Never fails: missing or unparsable keys are `None`.
    pub fn read(root: &Path) -> Self {
        let dir = entry_dir(root, MODE_CONFIG_ID);
        ModeConfig {
            is_dark: read_key(&dir, "is_dark"),
            auto_switch: read_key(&dir, "auto_switch"),
        }
    }

    /// The mode COSMIC asks for, or `None` when the macOS appearance should decide (no readable
    /// `is_dark`, or `auto_switch` is on).
    pub fn cosmic_choice(&self) -> Option<Mode> {
        if self.auto_switch == Some(true) {
            return None;
        }
        self.is_dark.map(Mode::from_dark)
    }
}

/// The resolution rule (see the module docs). `macos_dark` is only called when COSMIC's config
/// does not decide.
pub fn resolve_mode(config: &ModeConfig, macos_dark: impl FnOnce() -> bool) -> Mode {
    config
        .cosmic_choice()
        .unwrap_or_else(|| Mode::from_dark(macos_dark()))
}

/// [`ModeConfig::read`] then [`resolve_mode`].
pub fn current_mode(root: &Path, macos_dark: impl FnOnce() -> bool) -> Mode {
    resolve_mode(&ModeConfig::read(root), macos_dark)
}

/// Whether the value of [`MACOS_STYLE_KEY`] means dark mode.
pub fn macos_style_is_dark(style: Option<&str>) -> bool {
    style.is_some_and(|style| style.trim().eq_ignore_ascii_case("dark"))
}

fn read_key<T: DeserializeOwned>(dir: &Path, key: &str) -> Option<T> {
    let text = fs::read_to_string(dir.join(key)).ok()?;
    ron::from_str(&text).ok()
}

/// A colour with sRGB components in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub red: f64,
    pub green: f64,
    pub blue: f64,
    pub alpha: f64,
}

impl Rgba {
    pub const BLACK: Rgba = Rgba::new(0.0, 0.0, 0.0, 1.0);
    pub const WHITE: Rgba = Rgba::new(1.0, 1.0, 1.0, 1.0);

    pub const fn new(red: f64, green: f64, blue: f64, alpha: f64) -> Self {
        Rgba {
            red,
            green,
            blue,
            alpha,
        }
    }

    /// An opaque grey.
    pub const fn grey(level: f64) -> Self {
        Rgba::new(level, level, level, 1.0)
    }

    pub const fn with_alpha(self, alpha: f64) -> Self {
        Rgba { alpha, ..self }
    }

    /// `amount` of `other` over this colour (0 = this, 1 = `other`), alpha included.
    pub const fn mix(self, other: Rgba, amount: f64) -> Self {
        let keep = 1.0 - amount;
        Rgba::new(
            self.red * keep + other.red * amount,
            self.green * keep + other.green * amount,
            self.blue * keep + other.blue * amount,
            self.alpha * keep + other.alpha * amount,
        )
    }

    /// `#RRGGBB` or `#RRGGBBAA` (the `#` is optional), as cosmic-config writes colours.
    pub fn from_hex(text: &str) -> Option<Self> {
        let hex = text.trim();
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |index: usize| {
            u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .ok()
                .map(|value| f64::from(value) / 255.0)
        };
        let alpha = if hex.len() == 8 { channel(3)? } else { 1.0 };
        Some(Rgba::new(channel(0)?, channel(1)?, channel(2)?, alpha))
    }

    /// WCAG relative luminance (alpha ignored).
    pub fn luminance(self) -> f64 {
        fn linear(channel: f64) -> f64 {
            let channel = channel.clamp(0.0, 1.0);
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear(self.red) + 0.7152 * linear(self.green) + 0.0722 * linear(self.blue)
    }

    /// Black or white, whichever contrasts more with this colour.
    pub fn contrasting_text(self) -> Rgba {
        // Above this luminance black has the higher WCAG contrast ratio.
        if self.luminance() > 0.179 {
            Rgba::BLACK
        } else {
            Rgba::WHITE
        }
    }

    fn is_valid(self) -> bool {
        [self.red, self.green, self.blue, self.alpha]
            .iter()
            .all(|channel| channel.is_finite())
    }
}

/// The user's accent colour from COSMIC's theme config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accent {
    /// Accent fill.
    pub base: Rgba,
    /// Text on the accent fill, when the config has it.
    pub on: Option<Rgba>,
}

/// Reads `accent` from COSMIC's theme config for `mode` (`<root>/com.system76.CosmicTheme.Dark/v1/accent`
/// or `.Light`). `None` when the file is missing or unreadable.
pub fn read_accent(root: &Path, mode: Mode) -> Option<Accent> {
    let text = fs::read_to_string(entry_dir(root, mode.theme_id()).join("accent")).ok()?;
    parse_accent(&text)
}

/// Parses the RON of a cosmic-theme `Component` (the `accent` key): its `base` and `on` colours,
/// each a hex string (`"#63D0DFFF"`, current cosmic-theme) or an `(red:, green:, blue:[, alpha:])`
/// struct (older cosmic-theme). Other fields are ignored.
pub fn parse_accent(text: &str) -> Option<Accent> {
    let component: ComponentRepr = ron::from_str(text).ok()?;
    let base = component.base.into_rgba()?;
    let on = component.on.and_then(ColorRepr::into_rgba);
    Some(Accent { base, on })
}

#[derive(Deserialize)]
struct ComponentRepr {
    base: ColorRepr,
    #[serde(default, deserialize_with = "present")]
    on: Option<ColorRepr>,
}

/// A present field is `Some`; `#[serde(default)]` makes a missing one `None`. (Plain RON needs
/// `Some(..)` around optional values, which cosmic-config does not write.)
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<ColorRepr>, D::Error> {
    ColorRepr::deserialize(deserializer).map(Some)
}

/// cosmic-theme's `ColorRepr`: a hex string, RGBA or RGB.
#[derive(Deserialize)]
#[serde(untagged)]
enum ColorRepr {
    Hex(String),
    Rgba {
        red: f64,
        green: f64,
        blue: f64,
        alpha: f64,
    },
    Rgb {
        red: f64,
        green: f64,
        blue: f64,
    },
}

impl ColorRepr {
    fn into_rgba(self) -> Option<Rgba> {
        let color = match self {
            ColorRepr::Hex(hex) => Rgba::from_hex(&hex)?,
            ColorRepr::Rgba {
                red,
                green,
                blue,
                alpha,
            } => Rgba::new(red, green, blue, alpha),
            ColorRepr::Rgb { red, green, blue } => Rgba::new(red, green, blue, 1.0),
        };
        color.is_valid().then_some(color)
    }
}

/// The stack tab bar's colours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabBarPalette {
    /// The bar behind the tabs: COSMIC `background.base`, slightly translucent.
    pub background: Rgba,
    /// Inactive tabs: COSMIC `background.component.base`.
    pub tab_inactive: Rgba,
    /// The active tab: COSMIC `accent.base`.
    pub tab_active: Rgba,
    /// The active tab of a stack without the focus: the accent mixed into the inactive
    /// tab ([`UNFOCUSED_ACCENT`]).  Its title and close glyph use `text` and `close`.
    pub tab_active_unfocused: Rgba,
    /// Title on an inactive tab: COSMIC `background.component.on`.
    pub text: Rgba,
    /// Title and close glyph on the active tab: COSMIC `accent.on`.
    pub text_active: Rgba,
    /// Close glyph on an inactive tab: COSMIC `palette.neutral_7`.
    pub close: Rgba,
}

/// Alpha of the bar behind the tabs.
const BACKGROUND_ALPHA: f64 = 0.94;

/// How much accent the active tab of an unfocused stack gets over the inactive tab colour.
pub const UNFOCUSED_ACCENT: f64 = 0.35;

impl TabBarPalette {
    /// From COSMIC's default dark theme.
    pub const DARK: TabBarPalette = TabBarPalette {
        background: Rgba::grey(0.1059).with_alpha(BACKGROUND_ALPHA),
        tab_inactive: Rgba::grey(0.1822),
        tab_active: Rgba::new(0.3882, 0.8157, 0.8745, 1.0),
        tab_active_unfocused: Rgba::grey(0.1822)
            .mix(Rgba::new(0.3882, 0.8157, 0.8745, 1.0), UNFOCUSED_ACCENT),
        text: Rgba::grey(0.7533),
        text_active: Rgba::BLACK,
        close: Rgba::grey(0.6196),
    };

    /// From COSMIC's default light theme.
    pub const LIGHT: TabBarPalette = TabBarPalette {
        background: Rgba::grey(0.8431).with_alpha(BACKGROUND_ALPHA),
        tab_inactive: Rgba::grey(0.9602),
        tab_active: Rgba::new(0.0, 0.3216, 0.3529, 1.0),
        tab_active_unfocused: Rgba::grey(0.9602)
            .mix(Rgba::new(0.0, 0.3216, 0.3529, 1.0), UNFOCUSED_ACCENT),
        text: Rgba::grey(0.1529),
        text_active: Rgba::WHITE,
        close: Rgba::grey(0.1804),
    };

    /// The default palette for a mode.
    pub const fn defaults(mode: Mode) -> Self {
        match mode {
            Mode::Dark => TabBarPalette::DARK,
            Mode::Light => TabBarPalette::LIGHT,
        }
    }

    /// The palette for a mode with the user's accent, if any. Without an `on` colour in the
    /// config the active tab's text is black or white, whichever contrasts more.
    pub fn new(mode: Mode, accent: Option<Accent>) -> Self {
        let mut palette = TabBarPalette::defaults(mode);
        if let Some(accent) = accent {
            palette.tab_active = accent.base.with_alpha(1.0);
            palette.tab_active_unfocused = palette
                .tab_inactive
                .mix(palette.tab_active, UNFOCUSED_ACCENT);
            palette.text_active = accent
                .on
                .unwrap_or_else(|| accent.base.contrasting_text())
                .with_alpha(1.0);
        }
        palette
    }

    /// [`current_mode`], [`read_accent`] and [`TabBarPalette::new`] in one call.
    pub fn current(root: &Path, macos_dark: impl FnOnce() -> bool) -> (Mode, Self) {
        let mode = current_mode(root, macos_dark);
        (mode, TabBarPalette::new(mode, read_accent(root, mode)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_key(root: &Path, id: &str, key: &str, value: &str) {
        let dir = entry_dir(root, id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(key), value).unwrap();
    }

    fn never() -> bool {
        panic!("the macOS appearance must not be read when COSMIC decides")
    }

    #[test]
    fn paths_follow_cosmic_config() {
        let root = config_root(Path::new("/Users/someone"));
        assert_eq!(
            root,
            Path::new("/Users/someone/Library/Application Support/cosmic")
        );
        assert_eq!(
            entry_dir(&root, MODE_CONFIG_ID),
            Path::new(
                "/Users/someone/Library/Application Support/cosmic/com.system76.CosmicTheme.Mode/v1"
            )
        );
        assert_eq!(Mode::Dark.theme_id(), "com.system76.CosmicTheme.Dark");
        assert_eq!(Mode::Light.theme_id(), "com.system76.CosmicTheme.Light");
    }

    #[test]
    fn cosmic_config_decides_when_auto_switch_is_off() {
        for (is_dark, auto_switch) in [(true, Some(false)), (false, Some(false))] {
            let config = ModeConfig {
                is_dark: Some(is_dark),
                auto_switch,
            };
            assert_eq!(resolve_mode(&config, never), Mode::from_dark(is_dark));
        }
    }

    #[test]
    fn missing_auto_switch_counts_as_off() {
        let config = ModeConfig {
            is_dark: Some(false),
            auto_switch: None,
        };
        assert_eq!(resolve_mode(&config, never), Mode::Light);
    }

    #[test]
    fn auto_switch_follows_macos() {
        let config = ModeConfig {
            is_dark: Some(true),
            auto_switch: Some(true),
        };
        assert_eq!(resolve_mode(&config, || false), Mode::Light);
        assert_eq!(resolve_mode(&config, || true), Mode::Dark);
    }

    #[test]
    fn no_is_dark_follows_macos() {
        for auto_switch in [None, Some(false), Some(true)] {
            let config = ModeConfig {
                is_dark: None,
                auto_switch,
            };
            assert_eq!(resolve_mode(&config, || true), Mode::Dark);
            assert_eq!(resolve_mode(&config, || false), Mode::Light);
        }
    }

    #[test]
    fn reads_the_mode_config_from_disk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert_eq!(ModeConfig::read(root), ModeConfig::default());
        assert_eq!(current_mode(root, || false), Mode::Light);
        assert_eq!(current_mode(root, || true), Mode::Dark);

        // cosmic-config writes pretty RON; a trailing newline or comment is fine.
        write_key(root, MODE_CONFIG_ID, "is_dark", "false\n");
        write_key(root, MODE_CONFIG_ID, "auto_switch", "  false // manual\n");
        assert_eq!(
            ModeConfig::read(root),
            ModeConfig {
                is_dark: Some(false),
                auto_switch: Some(false),
            }
        );
        assert_eq!(current_mode(root, never), Mode::Light);

        write_key(root, MODE_CONFIG_ID, "is_dark", "true");
        assert_eq!(current_mode(root, never), Mode::Dark);

        write_key(root, MODE_CONFIG_ID, "auto_switch", "true");
        assert_eq!(current_mode(root, || false), Mode::Light);
    }

    #[test]
    fn garbage_is_dark_follows_macos() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_key(root, MODE_CONFIG_ID, "auto_switch", "false");
        for garbage in ["", "yes", "1", "\"true\"", "(", "\u{fffd}"] {
            write_key(root, MODE_CONFIG_ID, "is_dark", garbage);
            assert_eq!(ModeConfig::read(root).is_dark, None, "{garbage:?}");
            assert_eq!(current_mode(root, || true), Mode::Dark);
            assert_eq!(current_mode(root, || false), Mode::Light);
        }
        // A directory where the key file should be is unreadable too.
        let dir = entry_dir(root, MODE_CONFIG_ID);
        fs::remove_file(dir.join("is_dark")).unwrap();
        fs::create_dir(dir.join("is_dark")).unwrap();
        assert_eq!(current_mode(root, || false), Mode::Light);
    }

    #[test]
    fn garbage_auto_switch_counts_as_off() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_key(root, MODE_CONFIG_ID, "is_dark", "false");
        write_key(root, MODE_CONFIG_ID, "auto_switch", "maybe");
        assert_eq!(current_mode(root, never), Mode::Light);
    }

    #[test]
    fn macos_style() {
        assert!(macos_style_is_dark(Some("Dark")));
        assert!(macos_style_is_dark(Some("dark ")));
        assert!(!macos_style_is_dark(Some("Light")));
        assert!(!macos_style_is_dark(Some("")));
        assert!(!macos_style_is_dark(None));
    }

    #[test]
    fn hex_colours() {
        assert_eq!(
            Rgba::from_hex("#63D0DFFF"),
            Some(Rgba::new(99.0 / 255.0, 208.0 / 255.0, 223.0 / 255.0, 1.0))
        );
        assert_eq!(
            Rgba::from_hex("00525a"),
            Some(Rgba::new(0.0, 82.0 / 255.0, 90.0 / 255.0, 1.0))
        );
        assert_eq!(
            Rgba::from_hex("#63D0DF80").map(|color| color.alpha),
            Some(128.0 / 255.0)
        );
        for bad in [
            "",
            "#",
            "#12345",
            "#1234567",
            "#GG0000",
            "#12345678FF",
            "#ééé",
        ] {
            assert_eq!(Rgba::from_hex(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn contrasting_text_matches_cosmic_defaults() {
        assert_eq!(
            TabBarPalette::DARK.tab_active.contrasting_text(),
            Rgba::BLACK
        );
        assert_eq!(
            TabBarPalette::LIGHT.tab_active.contrasting_text(),
            Rgba::WHITE
        );
        assert_eq!(Rgba::grey(0.9).contrasting_text(), Rgba::BLACK);
        assert_eq!(Rgba::grey(0.2).contrasting_text(), Rgba::WHITE);
    }

    #[test]
    fn parses_hex_accent() {
        // cosmic-theme's current format (ron::ser::to_string_pretty of a Component).
        let text = r##"(
    base: "#63D0DFFF",
    hover: "#63BAC6FF",
    pressed: "#3C737AFF",
    selected: "#63BAC6FF",
    selected_text: "#63D0DFFF",
    focus: "#63D0DFFF",
    divider: "#000000FF",
    on: "#000000FF",
    disabled: "#63D0DFFF",
    on_disabled: "#326870FF",
    border: "#63D0DFFF",
    disabled_border: "#63D0DF80",
)"##;
        let accent = parse_accent(text).unwrap();
        assert_eq!(accent.base, Rgba::from_hex("#63D0DF").unwrap());
        assert_eq!(accent.on, Some(Rgba::BLACK));
    }

    #[test]
    fn parses_struct_accent() {
        // Older cosmic-theme wrote palette's Srgba as a struct.
        let text = "(base: (red: 0.5, green: 0.25, blue: 1.0, alpha: 1.0), \
                    hover: (red: 0.1, green: 0.1, blue: 0.1, alpha: 1.0), \
                    on: (red: 1.0, green: 1.0, blue: 1.0))";
        let accent = parse_accent(text).unwrap();
        assert_eq!(accent.base, Rgba::new(0.5, 0.25, 1.0, 1.0));
        assert_eq!(accent.on, Some(Rgba::WHITE));

        let without_on = parse_accent("(base: \"#FF8800\")").unwrap();
        assert_eq!(without_on.on, None);
    }

    #[test]
    fn rejects_garbage_accent() {
        for bad in [
            "",
            "\"#63D0DF\"",
            "(hover: \"#63D0DF\")",
            "(base: \"#nothex\")",
            "(base: (red: 0.5))",
            "(base: (red: inf, green: 0.0, blue: 0.0))",
            "(base: 7)",
        ] {
            assert_eq!(parse_accent(bad), None, "{bad:?}");
        }
        // An unreadable `on` does not spoil the accent.
        let accent = parse_accent("(base: \"#FF8800\", on: \"#xyz\")").unwrap();
        assert_eq!(accent.on, None);
    }

    #[test]
    fn palette_uses_the_accent() {
        assert_eq!(TabBarPalette::new(Mode::Dark, None), TabBarPalette::DARK);
        assert_eq!(TabBarPalette::new(Mode::Light, None), TabBarPalette::LIGHT);

        let orange = Rgba::from_hex("#FF8800").unwrap();
        let palette = TabBarPalette::new(
            Mode::Light,
            Some(Accent {
                base: orange,
                on: None,
            }),
        );
        assert_eq!(palette.tab_active, orange);
        assert_eq!(
            palette.tab_active_unfocused,
            TabBarPalette::LIGHT
                .tab_inactive
                .mix(orange, UNFOCUSED_ACCENT)
        );
        assert_eq!(palette.text_active, Rgba::BLACK);
        assert_eq!(palette.background, TabBarPalette::LIGHT.background);
        assert_eq!(palette.tab_inactive, TabBarPalette::LIGHT.tab_inactive);
        assert_eq!(palette.text, TabBarPalette::LIGHT.text);
        assert_eq!(palette.close, TabBarPalette::LIGHT.close);

        let translucent = Rgba::new(0.1, 0.2, 0.3, 0.5);
        let palette = TabBarPalette::new(
            Mode::Dark,
            Some(Accent {
                base: translucent,
                on: Some(Rgba::grey(0.9).with_alpha(0.2)),
            }),
        );
        assert_eq!(palette.tab_active, translucent.with_alpha(1.0));
        assert_eq!(palette.text_active, Rgba::grey(0.9));
    }

    #[test]
    fn palettes_differ_where_it_matters() {
        let (dark, light) = (TabBarPalette::DARK, TabBarPalette::LIGHT);
        assert!(dark.background.luminance() < light.background.luminance());
        assert!(dark.tab_inactive.luminance() < light.tab_inactive.luminance());
        assert!(dark.text.luminance() > light.text.luminance());
        // Text stays readable on its tab.
        for palette in [dark, light] {
            let gap = |a: Rgba, b: Rgba| (a.luminance() - b.luminance()).abs();
            assert!(gap(palette.text, palette.tab_inactive) > 0.3);
            assert!(gap(palette.text_active, palette.tab_active) > 0.3);
            assert!(gap(palette.close, palette.tab_inactive) > 0.2);
            // The unfocused stack's active tab stands out from the bar and the other tabs,
            // and its title (`text`) stays readable.
            let muted = palette.tab_active_unfocused;
            assert!(gap(palette.text, muted) > 0.3);
            assert!(gap(muted, palette.tab_inactive) > 0.08);
            assert!(gap(muted, palette.background) > 0.1);
            assert_ne!(muted, palette.tab_active);
        }
    }

    #[test]
    fn mix_blends_each_channel() {
        let black = Rgba::BLACK;
        let white = Rgba::WHITE.with_alpha(0.0);
        assert_eq!(black.mix(white, 0.0), black);
        assert_eq!(black.mix(white, 1.0), white);
        assert_eq!(black.mix(white, 0.25), Rgba::new(0.25, 0.25, 0.25, 0.75));
    }

    #[test]
    fn current_palette_reads_mode_and_accent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert_eq!(
            TabBarPalette::current(root, || true),
            (Mode::Dark, TabBarPalette::DARK)
        );
        assert_eq!(
            TabBarPalette::current(root, || false),
            (Mode::Light, TabBarPalette::LIGHT)
        );

        // An accent in the dark theme only affects dark mode.
        write_key(root, DARK_THEME_ID, "accent", "(base: \"#FF8800FF\")");
        let (mode, palette) = TabBarPalette::current(root, || true);
        assert_eq!(mode, Mode::Dark);
        assert_eq!(palette.tab_active, Rgba::from_hex("#FF8800").unwrap());
        assert_eq!(
            TabBarPalette::current(root, || false).1,
            TabBarPalette::LIGHT
        );

        write_key(root, MODE_CONFIG_ID, "is_dark", "false");
        write_key(root, LIGHT_THEME_ID, "accent", "not ron");
        assert_eq!(
            TabBarPalette::current(root, never),
            (Mode::Light, TabBarPalette::LIGHT)
        );
    }
}
