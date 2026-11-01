// SPDX-License-Identifier: GPL-3.0-or-later
//! `cosmic-mac-session` entry point.
//!
//! Exit codes: `0` success, `1` the operation failed, `2` usage error.

use std::env;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use cosmic_mac_session::agents::Paths;
use cosmic_mac_session::args::{self, Command, Parsed};
use cosmic_mac_session::runner::SystemRunner;
use cosmic_mac_session::session::{self, Session};

const EXIT_OK: u8 = 0;
const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().skip(1).collect();
    let command = match args::parse(argv) {
        Ok(Parsed::Run(command)) => command,
        Ok(Parsed::Help(text)) => {
            print!("{text}");
            return ExitCode::from(EXIT_OK);
        }
        Ok(Parsed::Version) => {
            println!("cosmic-mac-session {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::from(EXIT_OK);
        }
        Err(error) => {
            eprintln!("cosmic-mac-session: {error}");
            eprintln!();
            eprintln!("{}", args::USAGE_SHORT);
            return ExitCode::from(EXIT_USAGE);
        }
    };

    match run(command) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) => {
            eprintln!("cosmic-mac-session: {error}");
            ExitCode::from(EXIT_FAILED)
        }
    }
}

fn run(command: Command) -> session::Result<()> {
    // SAFETY: getuid() has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        return Err(session::Error(
            "run as the logged-in user, not root: the agents belong to that user's \
             GUI session (gui/<uid>)"
                .into(),
        ));
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| session::Error("$HOME is not set to an absolute path".into()))?;

    let mut runner = SystemRunner;
    let mut stdout = io::stdout();
    let paths = Paths::new(home);

    match command {
        Command::Install { bin_dir, dry_run } => {
            let bin_dir = session::resolve_bin_dir(bin_dir.as_deref(), env::current_exe())?;
            Session::new(&mut runner, &mut stdout, paths, uid, dry_run).install(&bin_dir)
        }
        Command::Uninstall { dry_run } => {
            Session::new(&mut runner, &mut stdout, paths, uid, dry_run).uninstall()
        }
        Command::Status => {
            let status = Session::new(&mut runner, &mut stdout, paths, uid, false).status()?;
            print!("{}", status.render());
            Ok(())
        }
    }
}
