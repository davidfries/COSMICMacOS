// SPDX-License-Identifier: GPL-3.0-or-later
//! Fixed filesystem locations used by the daemon.
//!
//! Everything lives under `~/Library/Application Support/cosmic/`, which is
//! where cosmic-config stores the rest of COSMIC's settings on macOS.  The
//! socket path is deliberately **not** under `$TMPDIR`: non-interactive SSH
//! shells often have no `TMPDIR`, and `cosmic-mac-ctl` must find the daemon
//! regardless of how it was launched.

use std::path::PathBuf;

/// The user's home directory.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `~/Library/Application Support/cosmic`.
pub fn cosmic_dir() -> PathBuf {
    home_dir()
        .join("Library")
        .join("Application Support")
        .join("cosmic")
}

/// `~/Library/Application Support/cosmic/cosmic-mac-wm.sock`.
pub fn socket_path() -> PathBuf {
    cosmic_dir().join("cosmic-mac-wm.sock")
}

/// `~/Library/Application Support/cosmic/frame-journal.ron`.
pub fn journal_path() -> PathBuf {
    cosmic_dir().join("frame-journal.ron")
}

/// `~/Library/Application Support/cosmic/wm-session.ron`.
///
/// Remembers the work area and tiling order of the last session so a restart
/// can tell whether the previous tiling tree is still valid.
pub fn session_path() -> PathBuf {
    cosmic_dir().join("wm-session.ron")
}

/// `~/Library/Application Support/cosmic/com.system76.CosmicSettings.Shortcuts/custom.ron`.
pub fn keybindings_path() -> PathBuf {
    cosmic_dir()
        .join("com.system76.CosmicSettings.Shortcuts")
        .join("custom.ron")
}

/// `~/Library/Application Support/cosmic/com.system76.CosmicComp/config.ron`.
pub fn wm_config_path() -> PathBuf {
    cosmic_dir()
        .join("com.system76.CosmicComp")
        .join("config.ron")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_under_application_support() {
        let dir = cosmic_dir();
        assert!(dir.ends_with("Library/Application Support/cosmic"));
        assert!(socket_path().ends_with("cosmic-mac-wm.sock"));
        assert!(journal_path().ends_with("frame-journal.ron"));
        assert!(session_path().ends_with("wm-session.ron"));
        assert!(
            keybindings_path()
                .to_string_lossy()
                .contains("com.system76.CosmicSettings.Shortcuts/custom.ron")
        );
    }
}
