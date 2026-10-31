// SPDX-License-Identifier: GPL-3.0-or-later
//! Tiny throwaway IPC client used by the VM smoke test.
//!
//! ```sh
//! cargo run -q -p cosmic-mac-wm --example socket-probe -- list_windows
//! cargo run -q -p cosmic-mac-wm --example socket-probe -- \
//!     '{"request":"action","data":{"action":"focus","args":{"direction":"left"}}}'
//! ```
//!
//! It sends one line and prints the daemon's one-line JSON reply.  This is not
//! `cosmic-mac-ctl`; it exists so the WM can be tested independently.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use cosmic_mac_wm::paths;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argument = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "list_windows".into());
    let request = if argument.trim_start().starts_with('{') {
        argument
    } else {
        format!(r#"{{"request":"{argument}"}}"#)
    };

    let socket = std::env::args()
        .nth(2)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(paths::socket_path);
    let mut stream = UnixStream::connect(&socket)?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    print!("{reply}");
    Ok(())
}
