// SPDX-License-Identifier: GPL-3.0-or-later
//! cosmicMac launcher (spike): a COSMIC-launcher-style search overlay.
//!
//! One process per invocation: it opens a centred search window that takes
//! keyboard focus, searches the in-process plugins ([`plugins`]: calculator,
//! web search, the window manager's windows, applications) as you type, and
//! exits after an activation, on Escape, or when it loses focus. Startup
//! timing (process start → window visible → first frame → key) is logged at
//! info level to decide later between per-invocation and resident.
//!
//! Logging: `RUST_LOG` (default `warn,cosmic_mac_launcher=info`).

mod app;
mod appkit;
mod geometry;
mod icons;
mod matching;
mod plugin;
mod plugins;
#[cfg(test)]
mod test_support;

use std::time::Instant;

use cosmic::app::Settings;
use cosmic::iced;

fn main() -> iced::Result {
    let started = Instant::now();
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,cosmic_mac_launcher=info"),
    )
    .format_timestamp_millis()
    .init();
    log::info!("cosmic-mac-launcher starting");

    // No main window: the launcher opens its own window with the level and
    // visibility libcosmic's main-window settings cannot express.
    let settings = Settings::default()
        .no_main_window(true)
        .theme(cosmic::Theme::dark());

    cosmic::app::run::<app::Launcher>(settings, app::Flags { started })
}
