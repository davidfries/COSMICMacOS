// SPDX-License-Identifier: GPL-3.0-or-later
//! cosmicMac window-manager daemon.
//!
//! The crate is split into host-testable pieces:
//!
//! * [`keybindings`] — COSMIC `keybindings.ron` parsing and the embedded defaults.
//! * [`journal`] — crash-safe original-frame journal.
//! * [`session`] — work area and tiling order of the previous run, used to
//!   decide whether a restart may reuse the old tree order.
//! * [`rules`] — float-by-default classification and the managed-window filter.
//! * [`reserve`] — edge strips reserved from the work area for shell windows.
//! * [`constraints`] — per-window size constraints learned from the platform.
//! * [`commands`] — `Command` → platform calls, debounce and tab-bar updates.
//! * [`core`] — the single-threaded layout owner fed by platform events and IPC.
//! * [`daemon`] — the tokio event loop joining platform, keyboard and IPC.
//! * [`ipc`] / [`shell_state`] — the Unix-socket server and the shell-state
//!   stream behind `Request::Subscribe` (the menu-bar status item).
//! * [`macos`] / [`tabbar`] — the real backend and the Core Graphics tab bar.
//! * [`super_tap`] / [`launcher`] — Super-tap detection and the launcher's
//!   socket client (toggle, or start it on demand).

pub mod backend;
pub mod commands;
pub mod constraints;
pub mod core;
pub mod daemon;
pub mod ipc;
pub mod journal;
pub mod keybindings;
pub mod launcher;
pub mod macos;
pub mod paths;
pub mod reserve;
pub mod rules;
pub mod session;
pub mod shell_state;
pub mod super_tap;
pub mod tabbar;
