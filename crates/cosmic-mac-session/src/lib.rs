// SPDX-License-Identifier: GPL-3.0-or-later
//! Session installer for cosmicMac: installs, removes and reports the launchd
//! agents that run the shell in the user's GUI session.
//!
//! * [`agents`] is the list of agents and generates their plists.
//! * [`plist`] is a minimal XML property-list writer.
//! * [`runner`] puts every external command behind the [`runner::CommandRunner`]
//!   trait, so tests run against a scripted fake.
//! * [`session`] implements `install`/`uninstall`/`status`, including dry-run.
//! * [`args`] parses the command line.
//!
//! All logic takes the home directory and uid as parameters; only `main.rs`
//! reads `$HOME` and calls `getuid()`.

pub mod agents;
pub mod args;
pub mod plist;
pub mod runner;
pub mod session;
