// SPDX-License-Identifier: GPL-3.0-or-later
//! `cosmic-mac-ctl` entry point.
//!
//! Exit codes: `0` success, `1` daemon-reported error, `2` usage error, `3` IPC error or
//! daemon not running.

use std::env;
use std::io::Write;
use std::process::ExitCode;

use cosmic_mac_protocol::{Action, Request, Response, WindowId};

use cosmic_mac_ctl::args::{self, Command, Invocation, Parsed};
use cosmic_mac_ctl::client::{self, Client, Reply};
use cosmic_mac_ctl::output;

const EXIT_OK: i32 = 0;
const EXIT_DAEMON_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_IPC: i32 = 3;

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().skip(1).collect();
    let parsed = match args::parse(argv) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("cosmic-mac-ctl: {error}");
            eprintln!();
            eprintln!("{}", args::USAGE_SHORT);
            return ExitCode::from(EXIT_USAGE as u8);
        }
    };

    let code = match parsed {
        Parsed::Help(text) => {
            print!("{text}");
            EXIT_OK
        }
        Parsed::Version => {
            println!("cosmic-mac-ctl {}", env!("CARGO_PKG_VERSION"));
            EXIT_OK
        }
        Parsed::Run(invocation) => execute(invocation),
    };
    ExitCode::from(code as u8)
}

fn execute(invocation: Invocation) -> i32 {
    let path = match client::resolve_socket_path(invocation.socket.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cosmic-mac-ctl: {error}");
            return EXIT_IPC;
        }
    };
    let client = Client::new(path, invocation.timeout);
    let json = invocation.json;

    match invocation.command {
        Command::ListWindows { limit } => run_list_windows(&client, json, limit),
        Command::ListWorkspaces => run_list_workspaces(&client, json),
        Command::Tree { workspace } => run_tree(&client, json, workspace),
        Command::Action(action) => run_action(&client, json, &action),
        Command::Focus { wid } => run_focus(&client, json, wid),
        Command::Config => run_config(&client, json),
        Command::Restore => run_restore(&client, json),
        Command::Ping => run_ping(&client, json),
        Command::Watch => run_watch(&client, json),
    }
}

fn run_watch(client: &Client, json: bool) -> i32 {
    let mut subscription = match client.subscribe() {
        Ok(subscription) => subscription,
        Err(error) => return ipc_error(error),
    };
    let stdout = std::io::stdout();
    loop {
        let reply = match subscription.next_reply() {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                eprintln!("cosmic-mac-ctl: the daemon closed the subscription");
                return EXIT_IPC;
            }
            Err(error) => return ipc_error(error),
        };
        let Reply { response, data } = reply;
        let line = match response {
            Response::State(state) => {
                if json {
                    // One compact document per line, so the stream pipes into `jq`.
                    let encoded = match &data {
                        Some(value) => serde_json::to_string(value),
                        None => serde_json::to_string(&state),
                    };
                    match encoded {
                        Ok(text) => text,
                        Err(error) => {
                            eprintln!("cosmic-mac-ctl: failed to encode JSON output: {error}");
                            return EXIT_IPC;
                        }
                    }
                } else {
                    output::render_state(&state)
                }
            }
            Response::Error(message) => return daemon_error(&message),
            other => return unexpected("state", &other),
        };
        let mut out = stdout.lock();
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            // The reader went away (e.g. `watch | head -1`).
            return EXIT_OK;
        }
    }
}

fn run_list_windows(client: &Client, json: bool, limit: Option<usize>) -> i32 {
    let reply = match client.request(&Request::ListWindows) {
        Ok(reply) => reply,
        Err(error) => return ipc_error(error),
    };
    let Reply { response, data } = reply;
    match response {
        Response::Windows(windows) => {
            if json {
                print_payload_json(data.as_ref(), &windows)
            } else {
                println!("{}", output::render_windows(&windows, limit));
                EXIT_OK
            }
        }
        Response::Error(message) => daemon_error(&message),
        other => unexpected("windows", &other),
    }
}

fn run_list_workspaces(client: &Client, json: bool) -> i32 {
    let reply = match client.request(&Request::ListWorkspaces) {
        Ok(reply) => reply,
        Err(error) => return ipc_error(error),
    };
    let Reply { response, data } = reply;
    match response {
        Response::Workspaces(workspaces) => {
            if json {
                print_payload_json(data.as_ref(), &workspaces)
            } else {
                println!("{}", output::render_workspaces(&workspaces));
                EXIT_OK
            }
        }
        Response::Error(message) => daemon_error(&message),
        other => unexpected("workspaces", &other),
    }
}

fn run_tree(client: &Client, json: bool, workspace: Option<u32>) -> i32 {
    if let Some(target) = workspace {
        let reply = match client.request(&Request::ListWorkspaces) {
            Ok(reply) => reply,
            Err(error) => return ipc_error(error),
        };
        let Reply { response, .. } = reply;
        match response {
            Response::Workspaces(workspaces) => {
                let active = workspaces.iter().find(|workspace| workspace.active);
                match active {
                    Some(active) if active.index == target => {}
                    Some(_) => {
                        // Request::Tree has no workspace selector.  Composing the existing
                        // Workspace action keeps the protocol unchanged and is announced
                        // loudly so scripts are not surprised by the state change.
                        eprintln!(
                            "note: switching to workspace {target} \
                             (the IPC `tree` request only reports the active workspace)"
                        );
                        let reply = match client
                            .request(&Request::Action(Action::Workspace { index: target }))
                        {
                            Ok(reply) => reply,
                            Err(error) => return ipc_error(error),
                        };
                        match reply.response {
                            Response::Ok => {}
                            Response::Error(message) => return daemon_error(&message),
                            other => return unexpected("ok", &other),
                        }
                    }
                    None => {
                        eprintln!(
                            "cosmic-mac-ctl: protocol error: the daemon reported no active workspace"
                        );
                        return EXIT_IPC;
                    }
                }
            }
            Response::Error(message) => return daemon_error(&message),
            other => return unexpected("workspaces", &other),
        }
    }

    let reply = match client.request(&Request::Tree) {
        Ok(reply) => reply,
        Err(error) => return ipc_error(error),
    };
    let Reply { response, data } = reply;
    match response {
        Response::Tree(tree) => {
            if json {
                print_payload_json(data.as_ref(), &tree)
            } else {
                println!("{}", output::render_tree(tree.as_ref()));
                EXIT_OK
            }
        }
        Response::Error(message) => daemon_error(&message),
        other => unexpected("tree", &other),
    }
}

fn run_action(client: &Client, json: bool, action: &Action) -> i32 {
    let reply = match client.request(&Request::Action(action.clone())) {
        Ok(reply) => reply,
        Err(error) => return ipc_error(error),
    };
    match reply.response {
        Response::Ok => {
            if json {
                print_json(&serde_json::json!({ "ok": true }))
            } else {
                println!("ok");
                EXIT_OK
            }
        }
        Response::Error(message) => daemon_error(&message),
        other => unexpected("ok", &other),
    }
}

fn run_focus(client: &Client, json: bool, wid: WindowId) -> i32 {
    let reply = match client.request(&Request::Focus { wid }) {
        Ok(reply) => reply,
        Err(error) => return ipc_error(error),
    };
    match reply.response {
        Response::Ok => {
            if json {
                print_json(&serde_json::json!({ "ok": true, "wid": wid }))
            } else {
                println!("focused {wid}");
                EXIT_OK
            }
        }
        Response::Error(message) => daemon_error(&message),
        other => unexpected("ok", &other),
    }
}

fn run_config(client: &Client, json: bool) -> i32 {
    let reply = match client.request(&Request::Config) {
        Ok(reply) => reply,
        Err(error) => return ipc_error(error),
    };
    let Reply { response, data } = reply;
    match response {
        Response::Config(config) => {
            if json {
                print_payload_json(data.as_ref(), &config)
            } else {
                println!("{}", output::render_config(&config));
                EXIT_OK
            }
        }
        Response::Error(message) => daemon_error(&message),
        other => unexpected("config", &other),
    }
}

fn run_restore(client: &Client, json: bool) -> i32 {
    let reply = match client.request(&Request::Restore) {
        Ok(reply) => reply,
        Err(error) => return ipc_error(error),
    };
    match reply.response {
        Response::Ok => {
            if json {
                print_json(&serde_json::json!({ "ok": true }))
            } else {
                println!("ok: restore requested");
                EXIT_OK
            }
        }
        Response::Error(message) => daemon_error(&message),
        other => unexpected("ok", &other),
    }
}

fn run_ping(client: &Client, json: bool) -> i32 {
    // The protocol has no Ping variant; ListWindows is the cheapest existing query and also
    // proves that the daemon can produce a payload.
    let reply = match client.request(&Request::ListWindows) {
        Ok(reply) => reply,
        Err(error) => return ipc_error(error),
    };
    match reply.response {
        Response::Windows(windows) => {
            if json {
                print_json(&serde_json::json!({ "ok": true, "windows": windows.len() }))
            } else {
                println!("ok: daemon responding ({} windows)", windows.len());
                EXIT_OK
            }
        }
        Response::Error(message) => daemon_error(&message),
        other => unexpected("windows", &other),
    }
}

/// Print the daemon's raw `data` payload when present, otherwise a typed fallback.
fn print_payload_json<T: serde::Serialize>(data: Option<&serde_json::Value>, fallback: &T) -> i32 {
    match data {
        Some(value) => print_json(value),
        None => print_json(fallback),
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> i32 {
    match serde_json::to_string_pretty(value) {
        Ok(text) => {
            println!("{text}");
            EXIT_OK
        }
        Err(error) => {
            eprintln!("cosmic-mac-ctl: failed to encode JSON output: {error}");
            EXIT_IPC
        }
    }
}

fn ipc_error(error: client::ClientError) -> i32 {
    eprintln!("cosmic-mac-ctl: {error}");
    EXIT_IPC
}

fn daemon_error(message: &str) -> i32 {
    eprintln!("cosmic-mac-ctl: daemon error: {message}");
    EXIT_DAEMON_ERROR
}

fn unexpected(expected: &str, response: &Response) -> i32 {
    eprintln!(
        "cosmic-mac-ctl: protocol error: expected `{expected}`, got `{}`",
        response_name(response)
    );
    EXIT_IPC
}

fn response_name(response: &Response) -> &'static str {
    match response {
        Response::Windows(_) => "windows",
        Response::Workspaces(_) => "workspaces",
        Response::Tree(_) => "tree",
        Response::Ok => "ok",
        Response::Config(_) => "config",
        Response::Error(_) => "error",
        Response::State(_) => "state",
    }
}
