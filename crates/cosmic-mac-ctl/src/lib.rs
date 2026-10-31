// SPDX-License-Identifier: GPL-3.0-or-later
//! Command-line client for the cosmicMac window manager.
//!
//! The crate is split into three testable pieces:
//!
//! * [`client`] resolves the Unix socket path, opens one connection per request, writes a
//!   single JSON line and reads a single JSON line back.
//! * [`args`] parses the command line without a CLI framework, including the friendly
//!   `action <name> [args]` syntax and raw JSON actions.
//! * [`output`] renders the protocol payloads as aligned tables / trees for humans.
//!
//! `main.rs` only wires the three together and maps errors onto the documented exit codes:
//! `0` success, `1` daemon-reported error, `2` usage error, `3` IPC/daemon-unavailable error.

pub mod args;
pub mod client;
pub mod output;
