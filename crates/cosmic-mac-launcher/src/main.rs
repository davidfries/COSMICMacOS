// SPDX-License-Identifier: GPL-3.0-or-later
//! cosmicMac launcher: a resident COSMIC-launcher-style search overlay.
//!
//! The process stays running (launchd agent `com.cosmicmac.launcher`,
//! started hidden) and listens on
//! `~/Library/Application Support/cosmic/cosmic-mac-launcher.sock` for
//! `toggle`, `show` and `hide` lines ([`ipc`]; the window manager sends
//! `toggle` when Super is tapped). Showing opens a centred search window
//! that takes keyboard focus and searches the in-process plugins
//! ([`plugins`]: calculator, web search, terminal commands, Spotlight
//! files, the window manager's windows, applications) as you type; an
//! activation, Escape or losing focus hides it again. See [`app`] for why
//! hiding closes the window while the app, its GPU renderer and plugin data
//! stay warm.
//!
//! Single instance: a second launcher started with `--show` asks the first
//! one to show and exits (see [`ipc`]).
//!
//! Logging: `RUST_LOG` (default `warn,cosmic_mac_launcher=info`).

mod app;
mod appkit;
mod args;
mod geometry;
mod icons;
mod ipc;
mod matching;
mod plugin;
mod plugins;
#[cfg(test)]
mod test_support;
mod theme;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use cosmic::app::Settings;
use cosmic::iced::futures::channel::mpsc;
use cosmic_mac_protocol::launcher::{self as protocol, LauncherCommand};

/// `EX_USAGE` from sysexits.h.
const EXIT_USAGE: u8 = 64;

fn main() -> ExitCode {
    let started = Instant::now();
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,cosmic_mac_launcher=info"),
    )
    .format_timestamp_millis()
    .init();

    let options = match args::parse(std::env::args_os().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("cosmic-mac-launcher: {error}\n\n{}", args::USAGE);
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if options.help {
        println!("{}", args::USAGE);
        return ExitCode::SUCCESS;
    }

    let Some(home) = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
    else {
        eprintln!("cosmic-mac-launcher: HOME is not set");
        return ExitCode::FAILURE;
    };
    let socket = protocol::socket_path(&home);

    let instance = match ipc::claim(&socket) {
        Ok(ipc::Claim::Primary(instance)) => instance,
        Ok(ipc::Claim::Secondary) => return forward_to_primary(&socket, options.show),
        Err(error) => {
            log::error!("cannot listen on {}: {error}", socket.display());
            return ExitCode::FAILURE;
        }
    };
    log::info!(
        "cosmic-mac-launcher starting ({}), listening on {}",
        if options.show { "shown" } else { "hidden" },
        socket.display()
    );

    // Socket threads → the app's IPC subscription.
    let (sender, receiver) = mpsc::unbounded();
    let guard = match instance.serve(Arc::new(move |command| {
        sender.unbounded_send(command).is_ok()
    })) {
        Ok(guard) => guard,
        Err(error) => {
            log::error!("cannot serve {}: {error}", socket.display());
            return ExitCode::FAILURE;
        }
    };
    app::install_commands(receiver);
    theme::observe_appearance();
    let theme_mode = theme::resolve(Some(&home));
    log::info!("theme: {theme_mode}");

    // No main window: the launcher opens its own window with the level and
    // visibility libcosmic's main-window settings cannot express. This also
    // makes libcosmic run an iced daemon, which keeps running (and keeps its
    // renderer) while no window is open.
    let settings = Settings::default()
        .no_main_window(true)
        .theme(theme::cosmic_theme(theme_mode, Some(&home)));
    let result = cosmic::app::run::<app::Launcher>(
        settings,
        app::Flags {
            started,
            show: options.show,
            theme_mode,
        },
    );
    // Removes the socket while still holding the single-instance lock.
    drop(guard);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("launcher event loop failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Another launcher holds the lock: with `--show`, ask it to show.
fn forward_to_primary(socket: &std::path::Path, show: bool) -> ExitCode {
    if !show {
        log::info!("a launcher is already running; exiting");
        return ExitCode::SUCCESS;
    }
    match ipc::forward(socket, LauncherCommand::Show, ipc::FORWARD_PATIENCE) {
        Ok(()) => {
            log::info!("asked the running launcher to show");
            ExitCode::SUCCESS
        }
        Err(error) => {
            log::error!("cannot reach the running launcher: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    /// The bundle's `Info.plist` (also embedded by `build.rs`).
    fn info_plist() -> plist::Dictionary {
        plist::Value::from_reader(std::io::Cursor::new(include_bytes!("../Info.plist")))
            .expect("Info.plist parses")
            .into_dictionary()
            .expect("Info.plist is a dictionary")
    }

    fn string<'a>(info: &'a plist::Dictionary, key: &str) -> &'a str {
        info.get(key)
            .and_then(plist::Value::as_string)
            .unwrap_or_else(|| panic!("{key} is a string"))
    }

    #[test]
    fn info_plist_makes_an_agent_app_with_the_signing_identifier() {
        let info = info_plist();
        // scripts/sign.sh signs `<name>.app` as `com.cosmicmac.<name>`.
        assert_eq!(
            string(&info, "CFBundleIdentifier"),
            "com.cosmicmac.cosmic-mac-launcher"
        );
        assert_eq!(string(&info, "CFBundleExecutable"), "cosmic-mac-launcher");
        assert_eq!(string(&info, "CFBundlePackageType"), "APPL");
        assert_eq!(
            info.get("LSUIElement").and_then(plist::Value::as_boolean),
            Some(true)
        );
        assert_eq!(
            info.get("NSHighResolutionCapable")
                .and_then(plist::Value::as_boolean),
            Some(true)
        );
    }

    #[test]
    fn info_plist_version_matches_the_crate() {
        let info = info_plist();
        assert_eq!(string(&info, "CFBundleVersion"), env!("CARGO_PKG_VERSION"));
        assert_eq!(
            string(&info, "CFBundleShortVersionString"),
            env!("CARGO_PKG_VERSION")
        );
    }
}
