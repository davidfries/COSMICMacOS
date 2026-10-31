// SPDX-License-Identifier: GPL-3.0-or-later
//! Layout session snapshot.
//!
//! The daemon does not persist the tiling tree itself: window frames are
//! restored by the crash journal, and the tree is rebuilt from a fresh window
//! enumeration on every start.  What it does persist is the *work area* the
//! tree was built for and the order in which its tiles were created, because:
//!
//! * reusing the recorded order keeps tiling stable across a restart with the
//!   same display geometry, and
//! * a changed work area (leaving fullscreen, plugging in a display, the VM
//!   resizing) invalidates that order: the previous session's tree slots were
//!   sized for a different screen, so keeping them parked Finder/Notes in tiny
//!   corner tiles.  In that case the daemon discards the order and rebuilds
//!   from a fresh front-to-back enumeration.
//!
//! The snapshot lives next to the frame journal as `wm-session.ron`.

use std::io::Write;
use std::path::{Path, PathBuf};

use cosmic_mac_protocol::{Rect, WindowId};
use serde::{Deserialize, Serialize};

/// What the daemon remembers about the previous run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    /// Work area the tiling tree was built for.
    #[serde(default)]
    pub work_area: Option<Rect>,
    /// Managed tiled windows in the order the tree was built (outermost tile
    /// first).  Floating windows are not part of a tiling tree and are omitted.
    #[serde(default)]
    pub tiled_order: Vec<WindowId>,
}

/// Load/save helper for [`SessionState`].
#[derive(Debug)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    /// A store backed by `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        SessionStore { path: path.into() }
    }

    /// Snapshot path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the previous snapshot.  A missing file is normal (first run); a
    /// corrupt one is logged and ignored rather than failing startup.
    pub fn load(&self) -> Option<SessionState> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                log::warn!("cannot read {}: {error}", self.path.display());
                return None;
            }
        };
        match ron::from_str(&text) {
            Ok(state) => Some(state),
            Err(error) => {
                log::warn!(
                    "cannot parse {}: {error}; rebuilding the tiling tree from scratch",
                    self.path.display()
                );
                None
            }
        }
    }

    /// Persist a snapshot atomically.  Failures are logged, never fatal: the
    /// session file is an optimisation, not the crash journal.
    pub fn save(&self, state: &SessionState) {
        let text = match ron::ser::to_string_pretty(state, ron::ser::PrettyConfig::default()) {
            Ok(text) => text,
            Err(error) => {
                log::warn!("cannot serialise the layout session: {error}");
                return;
            }
        };
        if let Some(parent) = self.path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            log::warn!("cannot create {}: {error}", parent.display());
            return;
        }
        let temporary = self.path.with_extension("ron.tmp");
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.path)
        })();
        if let Err(error) = result {
            log::warn!("cannot write {}: {error}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("wm-session.ron"));
        assert_eq!(store.load(), None);
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("wm-session.ron"));
        let state = SessionState {
            work_area: Some(Rect::new(0.0, 25.0, 1440.0, 875.0)),
            tiled_order: vec![3, 1, 2],
        };
        store.save(&state);
        assert_eq!(store.load(), Some(state));
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wm-session.ron");
        std::fs::write(&path, "not ron").unwrap();
        let store = SessionStore::new(&path);
        assert_eq!(store.load(), None);
    }
}
