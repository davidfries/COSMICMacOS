// SPDX-License-Identifier: GPL-3.0-or-later
//! cosmicMac window-manager daemon.
//!
//! The crate is split into host-testable pieces:
//!
//! * [`keybindings`] — COSMIC `keybindings.ron` parsing and the embedded defaults.
//! * [`journal`] — crash-safe original-frame journal.
//! * [`rules`] — float-by-default classification and the managed-window filter.
//! * [`commands`] — `Command` → platform calls, debounce and tab-bar updates.
//! * [`core`] — the single-threaded layout owner fed by platform events and IPC.
//! * [`daemon`] — the tokio event loop joining platform, keyboard and IPC.
//! * [`macos`] / [`tabbar`] — the real backend and the Core Graphics tab bar.

pub mod backend;
pub mod commands;
pub mod core;
pub mod daemon;
pub mod ipc;
pub mod journal;
pub mod keybindings;
pub mod macos;
pub mod paths;
pub mod rules;
pub mod tabbar;
