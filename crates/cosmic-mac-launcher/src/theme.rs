// SPDX-License-Identifier: GPL-3.0-or-later
//! Dark or light for the launcher, by the rule in [`cosmic_mac_protocol::theme`]: COSMIC's
//! `com.system76.CosmicTheme.Mode` when it has a readable `is_dark` and `auto_switch` is off,
//! otherwise the macOS appearance.
//!
//! libcosmic alone does not do this on macOS. It reads `is_dark` (ignoring `auto_switch`, which
//! cosmic-settings-daemon handles on Linux) and knows nothing of the macOS appearance (its
//! portal path is Linux-only). Its theme loading is also wrong for light mode without a
//! system-wide COSMIC install: cosmic-config fills missing keys from `Theme::default()`, which
//! is the *dark* theme, so `cosmic::theme::system_light()` on an empty or missing
//! `com.system76.CosmicTheme.Light` is dark, and libcosmic's own subscription to that config
//! pushes the same dark entry over a `ThemeType::System` theme. (On Linux the defaults ship in
//! `/usr/share/cosmic`.)
//!
//! So [`cosmic_theme`] loads the theme itself ([`load_theme`]: libcosmic's default for the mode
//! with the user's keys laid over it) and hands it over as `ThemeType::Custom`, which libcosmic
//! never replaces from its Mode or theme configs. The app re-resolves the mode (see `app.rs`) on
//! every show, on the macOS appearance notification ([`observe_appearance`]) and on Mode config
//! changes, and reloads the theme when the mode's theme config changes.

use std::fs;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, PoisonError};

use block2::RcBlock;
use cosmic::cosmic_config::{Config, CosmicConfigEntry};
use cosmic::cosmic_theme::Theme as CosmicTheme;
use cosmic::iced::futures::channel::mpsc;
use cosmic::iced::futures::stream::{self, BoxStream, StreamExt};
use cosmic_mac_protocol::theme::{self, Mode};
use objc2_foundation::{NSDistributedNotificationCenter, NSNotification, NSString, NSUserDefaults};

/// The mode for a home directory (`None`: only the macOS appearance).
pub fn resolve(home: Option<&Path>) -> Mode {
    match home {
        Some(home) => theme::current_mode(&theme::config_root(home), macos_prefers_dark),
        None => Mode::from_dark(macos_prefers_dark()),
    }
}

/// The macOS appearance: `AppleInterfaceStyle` from the user defaults, whose search list
/// includes the global domain.
pub fn macos_prefers_dark() -> bool {
    let key = NSString::from_str(theme::MACOS_STYLE_KEY);
    let style = NSUserDefaults::standardUserDefaults()
        .stringForKey(&key)
        .map(|style| style.to_string());
    theme::macos_style_is_dark(style.as_deref())
}

/// COSMIC's theme for a mode ([`load_theme`]) as a custom libcosmic theme, so libcosmic's own
/// Mode and theme-config updates leave it alone.
pub fn cosmic_theme(mode: Mode, home: Option<&Path>) -> cosmic::Theme {
    cosmic::Theme::custom(Arc::new(load_theme(mode, home)))
}

/// libcosmic's built-in theme for `mode` with every key found in the user's
/// `com.system76.CosmicTheme.Dark/v1` (or `.Light`) laid over it. Unreadable keys keep the
/// default (logged). `home: None` gives the built-in theme.
pub fn load_theme(mode: Mode, home: Option<&Path>) -> CosmicTheme {
    let mut cosmic = match mode {
        Mode::Dark => CosmicTheme::dark_default(),
        Mode::Light => CosmicTheme::light_default(),
    };
    let Some(home) = home else {
        return cosmic;
    };
    let root = theme::config_root(home);
    let Ok(entries) = fs::read_dir(theme::entry_dir(&root, mode.theme_id())) else {
        return cosmic;
    };
    // One file per key; skip cosmic-config's `.atomicwrite*` temporaries.
    let keys: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    if keys.is_empty() {
        return cosmic;
    }
    // cosmic-config adds the `cosmic` component itself.
    let Some(config_dir) = root.parent() else {
        return cosmic;
    };
    match Config::with_custom_path(mode.theme_id(), theme::CONFIG_VERSION, config_dir.into()) {
        Ok(config) => {
            let (errors, _changed) = cosmic.update_keys(&config, &keys);
            for error in errors.iter().filter(|error| error.is_err()) {
                log::warn!("{} theme config: {error}", mode);
            }
        }
        Err(error) => log::warn!("{} theme config: {error}", mode),
    }
    cosmic
}

/// Appearance-change notifications, handed from [`observe_appearance`] to the subscription.
static APPEARANCE: Mutex<Option<mpsc::UnboundedReceiver<()>>> = Mutex::new(None);

/// Observes macOS's `AppleInterfaceThemeChangedNotification` for the life of the process. Call
/// on the main thread before the app runs; delivery happens on the main run loop. While the app
/// is inactive (hidden), AppKit may hold distributed notifications back until it is activated;
/// a show re-resolves the mode anyway.
pub fn observe_appearance() {
    let (sender, receiver) = mpsc::unbounded();
    let block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
        let _ = sender.unbounded_send(());
    });
    let name = NSString::from_str(theme::MACOS_THEME_CHANGED_NOTIFICATION);
    let center = NSDistributedNotificationCenter::defaultCenter();
    // SAFETY: no object filter and no queue, so the block runs on the delivering (main)
    // thread; it only sends on a channel, which is thread-safe.
    let observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(Some(&name), None, None, &block)
    };
    // The registration stays for the life of the process.
    std::mem::forget(observer);
    *APPEARANCE.lock().unwrap_or_else(PoisonError::into_inner) = Some(receiver);
}

/// One item per appearance change. Taken once, by the subscription that is part of every
/// `Launcher::subscription`.
pub fn appearance_changes() -> BoxStream<'static, ()> {
    let receiver = APPEARANCE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    match receiver {
        Some(receiver) => receiver.boxed(),
        None => stream::pending().boxed(),
    }
}

#[cfg(test)]
mod tests {
    use cosmic::cosmic_config;
    use cosmic::cosmic_theme::palette::Srgba;
    use cosmic::cosmic_theme::{self, ThemeMode};
    use cosmic::theme::ThemeType;
    use cosmic_mac_protocol::theme::{ModeConfig, Rgba, TabBarPalette, read_accent};

    use super::*;
    use crate::test_support::TempDir;

    fn rgba(color: Srgba) -> Rgba {
        Rgba::new(
            f64::from(color.red),
            f64::from(color.green),
            f64::from(color.blue),
            f64::from(color.alpha),
        )
    }

    fn assert_close(ours: Rgba, cosmic: Srgba, what: &str) {
        let cosmic = rgba(cosmic);
        let close = [
            (ours.red, cosmic.red),
            (ours.green, cosmic.green),
            (ours.blue, cosmic.blue),
        ]
        .iter()
        .all(|(a, b)| (a - b).abs() < 0.002);
        assert!(close, "{what}: ours {ours:?}, COSMIC {cosmic:?}");
    }

    /// The tab bar's built-in palettes are COSMIC's default themes.
    #[test]
    fn tab_bar_palette_matches_cosmic_defaults() {
        for (palette, cosmic) in [
            (TabBarPalette::DARK, cosmic_theme::Theme::dark_default()),
            (TabBarPalette::LIGHT, cosmic_theme::Theme::light_default()),
        ] {
            let background = cosmic.background(false);
            assert_close(palette.background, background.base, "background");
            assert_close(
                palette.tab_inactive,
                background.component.base,
                "tab_inactive",
            );
            assert_close(palette.tab_active, cosmic.accent.base, "tab_active");
            assert_close(palette.text, background.component.on, "text");
            assert_close(palette.text_active, cosmic.accent.on, "text_active");
            assert_close(palette.close, cosmic.palette.neutral_7, "close");
        }
    }

    /// What cosmic-config writes is what the shared reader parses.
    #[test]
    fn reads_what_cosmic_config_writes() {
        let temp = TempDir::new("theme");
        // cosmic-config puts a custom path's entries under `<path>/cosmic`.
        let root = temp.path().join("cosmic");
        let config = |id| Config::with_custom_path(id, theme::CONFIG_VERSION, temp.path().into());

        let mode = config(theme::MODE_CONFIG_ID).unwrap();
        for (is_dark, auto_switch) in [(false, false), (true, true)] {
            ThemeMode {
                is_dark,
                auto_switch,
            }
            .write_entry(&mode)
            .unwrap();
            assert_eq!(
                ModeConfig::read(&root),
                ModeConfig {
                    is_dark: Some(is_dark),
                    auto_switch: Some(auto_switch),
                }
            );
        }

        let dark = cosmic_theme::Theme::dark_default();
        dark.write_entry(&config(theme::DARK_THEME_ID).unwrap())
            .unwrap();
        let accent = read_accent(&root, Mode::Dark).expect("dark accent");
        assert_close(accent.base, dark.accent.base, "default accent");
        assert_close(accent.on.expect("accent.on"), dark.accent.on, "accent.on");

        // Not `with_accent`: it reads the user's ThemeBuilder config (and
        // creates its directories) in the real home.
        let mut custom = cosmic_theme::Theme::light_default();
        custom.accent.base = Srgba::new(0.9, 0.3, 0.1, 1.0);
        custom.accent.on = Srgba::new(0.0, 0.0, 0.0, 1.0);
        custom
            .write_entry(&config(theme::LIGHT_THEME_ID).unwrap())
            .unwrap();
        let accent = read_accent(&root, Mode::Light).expect("light accent");
        assert_close(accent.base, custom.accent.base, "custom accent");
        assert_eq!(accent.on, Some(Rgba::BLACK));
        assert_eq!(
            TabBarPalette::new(Mode::Light, Some(accent)).tab_active,
            accent.base
        );
    }

    /// Regression: libcosmic fills missing keys from its dark default, so
    /// `system_light()` without a Light config (as on a fresh Mac) was dark.
    #[test]
    fn light_theme_without_config_is_light() {
        let temp = TempDir::new("light");
        let root = theme::config_root(temp.path());
        // No config at all, then the empty directories libcosmic creates.
        for create in [false, true] {
            if create {
                for mode in [Mode::Dark, Mode::Light] {
                    std::fs::create_dir_all(theme::entry_dir(&root, mode.theme_id())).unwrap();
                }
            }
            for (mode, default) in [
                (Mode::Dark, CosmicTheme::dark_default()),
                (Mode::Light, CosmicTheme::light_default()),
            ] {
                let loaded = load_theme(mode, Some(temp.path()));
                assert_eq!(loaded.is_dark, mode.is_dark(), "{mode}");
                assert_eq!(loaded, default, "{mode}");
            }
        }
        assert_eq!(load_theme(Mode::Light, None), CosmicTheme::light_default());
    }

    /// Keys the user has are laid over the mode's default, not the dark one.
    #[test]
    fn user_keys_override_the_mode_default() {
        let temp = TempDir::new("keys");
        let config_dir = temp.path().join("Library/Application Support");
        let config =
            Config::with_custom_path(theme::LIGHT_THEME_ID, theme::CONFIG_VERSION, config_dir)
                .unwrap();
        let mut accent = CosmicTheme::light_default().accent;
        accent.base = Srgba::new(0.9, 0.3, 0.1, 1.0);
        cosmic_config::ConfigSet::set(&config, "accent", accent.clone()).unwrap();
        // A broken key keeps its default.
        let dir = theme::entry_dir(&theme::config_root(temp.path()), theme::LIGHT_THEME_ID);
        std::fs::write(dir.join("corner_radii"), "not ron").unwrap();

        let loaded = load_theme(Mode::Light, Some(temp.path()));
        // cosmic-config stores colours as 8-bit hex.
        assert_close(rgba(loaded.accent.base), accent.base, "accent");
        assert_ne!(loaded.accent, CosmicTheme::light_default().accent);
        assert!(!loaded.is_dark);
        let light = CosmicTheme::light_default();
        assert_eq!(loaded.background(false), light.background(false));
        assert_eq!(loaded.corner_radii, light.corner_radii);
        // The dark theme is unaffected.
        assert_eq!(
            load_theme(Mode::Dark, Some(temp.path())),
            CosmicTheme::dark_default()
        );
    }

    /// A custom theme: libcosmic replaces `ThemeType::System` themes from its
    /// own Mode and theme-config subscriptions, never custom ones.
    #[test]
    fn theme_is_custom_and_matches_the_mode() {
        let temp = TempDir::new("custom");
        for mode in [Mode::Dark, Mode::Light] {
            let theme = cosmic_theme(mode, Some(temp.path()));
            assert_eq!(theme.cosmic().is_dark, mode.is_dark());
            assert!(
                matches!(theme.theme_type, ThemeType::Custom(_)),
                "{:?}",
                theme.theme_type
            );
        }
    }

    #[test]
    fn macos_appearance_matches_defaults() {
        let output = std::process::Command::new("/usr/bin/defaults")
            .args(["read", "-g", theme::MACOS_STYLE_KEY])
            .output()
            .expect("run defaults");
        let style = String::from_utf8_lossy(&output.stdout);
        let expected = output.status.success() && theme::macos_style_is_dark(Some(&style));
        assert_eq!(macos_prefers_dark(), expected, "defaults said {style:?}");
    }

    #[test]
    fn resolves_from_the_home_directory() {
        let temp = TempDir::new("mode");
        let dir = theme::entry_dir(&theme::config_root(temp.path()), theme::MODE_CONFIG_ID);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("is_dark"), "false").unwrap();
        assert_eq!(resolve(Some(temp.path())), Mode::Light);
        std::fs::write(dir.join("is_dark"), "true").unwrap();
        assert_eq!(resolve(Some(temp.path())), Mode::Dark);
        std::fs::write(dir.join("auto_switch"), "true").unwrap();
        assert_eq!(
            resolve(Some(temp.path())),
            Mode::from_dark(macos_prefers_dark())
        );
        assert_eq!(resolve(None), Mode::from_dark(macos_prefers_dark()));
    }
}
