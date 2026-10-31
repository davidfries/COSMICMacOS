// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end tests for `cosmic-mac-ctl` against a mock Unix-socket daemon.

mod common;

use std::os::unix::net::UnixListener;

use common::{
    MockDaemon, TempDir, cli, config_payload, config_reply, error_reply, exit_code, ok_reply,
    reply, run, run_at, run_env, stderr, stdout, tree_payload, tree_reply, windows_payload,
    windows_reply, workspaces_payload, workspaces_reply,
};
use serde_json::json;

fn mock(replies: Vec<String>) -> MockDaemon {
    MockDaemon::start(replies)
}

fn assert_ok(output: &std::process::Output) {
    assert_eq!(
        exit_code(output),
        0,
        "expected success\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn assert_exit(output: &std::process::Output, expected: i32) {
    assert_eq!(
        exit_code(output),
        expected,
        "expected exit {expected}\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn assert_request(daemon: &MockDaemon, expected: serde_json::Value) {
    assert_eq!(
        daemon.request_values(),
        vec![expected],
        "recorded requests: {:?}",
        daemon.requests()
    );
}

// ---------------------------------------------------------------------------------------------
// Help, version, usage
// ---------------------------------------------------------------------------------------------

#[test]
fn global_help_lists_commands_and_exit_zero() {
    let output = cli().arg("--help").output().unwrap();
    assert_ok(&output);
    let text = stdout(&output);
    for needle in [
        "list-windows",
        "list-workspaces",
        "tree",
        "action",
        "focus",
        "config",
        "restore",
        "ping",
        "--socket",
        "--timeout-ms",
        "COSMIC_MAC_WM_SOCKET",
        "Exit codes",
    ] {
        assert!(text.contains(needle), "help missing `{needle}`:\n{text}");
    }
}

#[test]
fn action_help_documents_every_friendly_action() {
    let output = cli().args(["action", "--help"]).output().unwrap();
    assert_ok(&output);
    let text = stdout(&output);
    for action in [
        "focus <dir>",
        "focus-window <WID>",
        "focus-next",
        "focus-previous",
        "move <dir>",
        "swap-window",
        "toggle-orientation",
        "resize <dir>",
        "resize-mode",
        "maximize",
        "toggle-tiling",
        "toggle-window-floating",
        "toggle-stacking",
        "unstack",
        "workspace <N>",
        "move-to-workspace <N>",
        "next-workspace",
        "previous-workspace",
        "last-workspace",
        "move-to-next-workspace",
        "move-to-previous-workspace",
        "move-to-last-workspace",
        "close",
    ] {
        assert!(
            text.contains(action),
            "action help missing `{action}`:\n{text}"
        );
    }
}

#[test]
fn per_command_help_is_available() {
    for (command, needle) in [
        ("list-windows", "WID, PID, APP"),
        ("list-workspaces", "INDEX, NAME"),
        ("tree", "no workspace selector"),
        ("focus", "Request::Focus"),
        ("config", "active_hint"),
        ("restore", "frame journal"),
        ("ping", "no dedicated ping request"),
    ] {
        let output = cli().args([command, "--help"]).output().unwrap();
        assert_ok(&output);
        let text = stdout(&output);
        assert!(
            text.contains(needle),
            "`{command} --help` missing `{needle}`:\n{text}"
        );
    }
}

#[test]
fn version_prints_the_crate_version() {
    let output = cli().arg("--version").output().unwrap();
    assert_ok(&output);
    assert!(stdout(&output).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_command_is_a_usage_error() {
    let output = cli().arg("frobnicate").output().unwrap();
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("unknown command `frobnicate`"));
    assert!(stderr(&output).contains("Usage: cosmic-mac-ctl"));
}

#[test]
fn extra_arguments_are_usage_errors() {
    let output = cli().args(["config", "extra"]).output().unwrap();
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("unexpected argument"));
}

#[test]
fn invalid_timeout_is_a_usage_error() {
    let output = cli()
        .args(["--timeout-ms", "0", "list-windows"])
        .output()
        .unwrap();
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("timeout"));
}

// ---------------------------------------------------------------------------------------------
// list-windows
// ---------------------------------------------------------------------------------------------

#[test]
fn list_windows_renders_an_aligned_table() {
    let daemon = mock(vec![windows_reply()]);
    let output = run(&daemon, &["list-windows"]);
    assert_ok(&output);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "header plus two rows:\n{text}");
    assert!(lines[0].contains("WID"));
    assert!(lines[0].contains("FRAME"));
    assert!(text.contains("com.apple.TextEdit"));
    assert!(text.contains("notes.txt"));
    assert!(text.contains("0,0 800x600"));
    assert!(text.contains("800,0 800x600"));
    assert!(text.contains("tab"));
    assert_request(&daemon, json!({"request": "list_windows"}));
}

#[test]
fn list_windows_json_is_the_protocol_payload() {
    let daemon = mock(vec![windows_reply()]);
    let output = run(&daemon, &["list-windows", "--json"]);
    assert_ok(&output);
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value, windows_payload());
}

#[test]
fn json_output_preserves_fields_the_cli_does_not_know() {
    let mut payload = windows_payload();
    payload[0]["future_field"] = json!({"nested": 1});
    let daemon = mock(vec![reply("windows", payload)]);
    let output = run(&daemon, &["list-windows", "--json"]);
    assert_ok(&output);
    assert!(
        stdout(&output).contains("future_field"),
        "raw server payload should not be re-encoded:\n{}",
        stdout(&output)
    );
}

#[test]
fn list_windows_limit_truncates_rows() {
    let daemon = mock(vec![windows_reply()]);
    let output = run(&daemon, &["list-windows", "--limit", "1"]);
    assert_ok(&output);
    assert_eq!(stdout(&output).lines().count(), 2);

    let daemon = mock(vec![windows_reply()]);
    let output = run(&daemon, &["list-windows", "--limit=0"]);
    assert_ok(&output);
    assert_eq!(stdout(&output).lines().count(), 1);
    assert!(stdout(&output).contains("WID"));
}

#[test]
fn list_windows_empty_payload_has_a_friendly_message() {
    let daemon = mock(vec![reply("windows", json!([]))]);
    let output = run(&daemon, &["list-windows"]);
    assert_ok(&output);
    assert!(stdout(&output).contains("no managed windows"));
}

// ---------------------------------------------------------------------------------------------
// list-workspaces
// ---------------------------------------------------------------------------------------------

#[test]
fn list_workspaces_renders_a_table() {
    let daemon = mock(vec![workspaces_reply()]);
    let output = run(&daemon, &["list-workspaces"]);
    assert_ok(&output);
    let text = stdout(&output);
    assert!(text.contains("INDEX"));
    assert!(text.contains("ACTIVE"));
    assert!(text.contains("yes"));
    assert!(text.contains("1"));
    assert_request(&daemon, json!({"request": "list_workspaces"}));
}

#[test]
fn list_workspaces_json_is_the_protocol_payload() {
    let daemon = mock(vec![workspaces_reply()]);
    let output = run(&daemon, &["list-workspaces", "--json"]);
    assert_ok(&output);
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value, workspaces_payload());
}

// ---------------------------------------------------------------------------------------------
// tree
// ---------------------------------------------------------------------------------------------

#[test]
fn tree_renders_groups_windows_and_stacks() {
    let daemon = mock(vec![tree_reply()]);
    let output = run(&daemon, &["tree"]);
    assert_ok(&output);
    let text = stdout(&output);
    assert!(text.contains("group horizontal [0,0 1600x900]"));
    assert!(text.contains("window 101 \"notes.txt\""));
    assert!(text.contains("stack [800,0 800x900] active 1/2"));
    assert!(text.contains("* 202 \"bash\""));
    assert!(text.contains("  203 \"zsh\""));
    assert_request(&daemon, json!({"request": "tree"}));
}

#[test]
fn tree_json_is_the_protocol_payload() {
    let daemon = mock(vec![tree_reply()]);
    let output = run(&daemon, &["tree", "--json"]);
    assert_ok(&output);
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value, tree_payload());
}

#[test]
fn tree_empty_payload_is_reported() {
    let daemon = mock(vec![reply("tree", serde_json::Value::Null)]);
    let output = run(&daemon, &["tree"]);
    assert_ok(&output);
    assert!(stdout(&output).contains("no tiling tree"));
}

#[test]
fn tree_workspace_switches_and_reports_the_gap() {
    let daemon = mock(vec![workspaces_reply(), ok_reply(), tree_reply()]);
    let output = run(&daemon, &["tree", "--workspace", "2"]);
    assert_ok(&output);
    assert!(stderr(&output).contains("switching to workspace 2"));
    assert_eq!(
        daemon.request_values(),
        vec![
            json!({"request": "list_workspaces"}),
            json!({"request": "action", "data": {"action": "workspace", "args": {"index": 2}}}),
            json!({"request": "tree"}),
        ]
    );
}

#[test]
fn tree_workspace_active_does_not_switch() {
    let daemon = mock(vec![workspaces_reply(), tree_reply()]);
    let output = run(&daemon, &["tree", "--workspace", "1"]);
    assert_ok(&output);
    assert!(!stderr(&output).contains("switching"));
    assert_eq!(
        daemon.request_values(),
        vec![
            json!({"request": "list_workspaces"}),
            json!({"request": "tree"})
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// focus
// ---------------------------------------------------------------------------------------------

#[test]
fn focus_sends_focus_request() {
    let daemon = mock(vec![ok_reply()]);
    let output = run(&daemon, &["focus", "42"]);
    assert_ok(&output);
    assert!(stdout(&output).contains("focused 42"));
    assert_eq!(
        daemon.requests(),
        vec![r#"{"request":"focus","data":{"wid":42}}"#]
    );
}

#[test]
fn focus_json_reports_ok_and_wid() {
    let daemon = mock(vec![ok_reply()]);
    let output = run(&daemon, &["focus", "--json", "42"]);
    assert_ok(&output);
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value, json!({"ok": true, "wid": 42}));
}

#[test]
fn focus_rejects_invalid_window_ids() {
    for args in [vec!["focus"], vec!["focus", "0"], vec!["focus", "abc"]] {
        let output = cli().args(&args).output().unwrap();
        assert_exit(&output, 2);
        assert!(stderr(&output).contains("window id"));
    }
}

// ---------------------------------------------------------------------------------------------
// action: every friendly spelling maps to the protocol Action
// ---------------------------------------------------------------------------------------------

#[test]
fn every_action_spelling_is_encoded_correctly() {
    let cases: Vec<(Vec<&str>, serde_json::Value)> = vec![
        (
            vec!["action", "focus", "left"],
            json!({"action": "focus", "args": {"direction": "left"}}),
        ),
        (
            vec!["action", "focus", "right"],
            json!({"action": "focus", "args": {"direction": "right"}}),
        ),
        (
            vec!["action", "focus", "up"],
            json!({"action": "focus", "args": {"direction": "up"}}),
        ),
        (
            vec!["action", "focus", "down"],
            json!({"action": "focus", "args": {"direction": "down"}}),
        ),
        (
            vec!["action", "focus-window", "99"],
            json!({"action": "focus_window", "args": {"wid": 99}}),
        ),
        (
            vec!["action", "focus-next"],
            json!({"action": "focus_next"}),
        ),
        (
            vec!["action", "focus-previous"],
            json!({"action": "focus_previous"}),
        ),
        (
            vec!["action", "move", "up"],
            json!({"action": "move", "args": {"direction": "up"}}),
        ),
        (
            vec!["action", "swap-window"],
            json!({"action": "swap_window"}),
        ),
        (
            vec!["action", "toggle-orientation"],
            json!({"action": "toggle_orientation"}),
        ),
        (
            vec!["action", "resize", "left"],
            json!({"action": "resize", "args": {"direction": "left"}}),
        ),
        (
            vec!["action", "resize-mode"],
            json!({"action": "resize_mode"}),
        ),
        (vec!["action", "maximize"], json!({"action": "maximize"})),
        (
            vec!["action", "toggle-tiling"],
            json!({"action": "toggle_tiling"}),
        ),
        (
            vec!["action", "toggle-window-floating"],
            json!({"action": "toggle_window_floating"}),
        ),
        (
            vec!["action", "toggle-stacking"],
            json!({"action": "toggle_stacking"}),
        ),
        (vec!["action", "unstack"], json!({"action": "unstack"})),
        (
            vec!["action", "workspace", "2"],
            json!({"action": "workspace", "args": {"index": 2}}),
        ),
        (
            vec!["action", "move-to-workspace", "3"],
            json!({"action": "move_to_workspace", "args": {"index": 3}}),
        ),
        (
            vec!["action", "next-workspace"],
            json!({"action": "next_workspace"}),
        ),
        (
            vec!["action", "previous-workspace"],
            json!({"action": "previous_workspace"}),
        ),
        (
            vec!["action", "last-workspace"],
            json!({"action": "last_workspace"}),
        ),
        (
            vec!["action", "move-to-next-workspace"],
            json!({"action": "move_to_next_workspace"}),
        ),
        (
            vec!["action", "move-to-previous-workspace"],
            json!({"action": "move_to_previous_workspace"}),
        ),
        (
            vec!["action", "move-to-last-workspace"],
            json!({"action": "move_to_last_workspace"}),
        ),
        (vec!["action", "close"], json!({"action": "close"})),
    ];

    let daemon = mock(vec![ok_reply(); cases.len()]);
    for (args, _) in &cases {
        let output = run(&daemon, args);
        assert_ok(&output);
        assert_eq!(stdout(&output).trim(), "ok");
    }

    let requests = daemon.request_values();
    assert_eq!(requests.len(), cases.len());
    for (index, (args, expected)) in cases.iter().enumerate() {
        assert_eq!(
            requests[index],
            json!({"request": "action", "data": expected}),
            "wrong encoding for {args:?}"
        );
    }
}

#[test]
fn action_focus_left_has_the_exact_contract_wire_format() {
    let daemon = mock(vec![ok_reply()]);
    let output = run(&daemon, &["action", "focus", "left"]);
    assert_ok(&output);
    assert_eq!(
        daemon.requests(),
        vec![r#"{"request":"action","data":{"action":"focus","args":{"direction":"left"}}}"#]
    );
}

#[test]
fn action_workspace_has_the_exact_contract_wire_format() {
    let daemon = mock(vec![ok_reply()]);
    let output = run(&daemon, &["action", "move-to-workspace", "3"]);
    assert_ok(&output);
    assert_eq!(
        daemon.requests(),
        vec![r#"{"request":"action","data":{"action":"move_to_workspace","args":{"index":3}}}"#]
    );
}

#[test]
fn action_direction_aliases_work_end_to_end() {
    let daemon = mock(vec![ok_reply(), ok_reply()]);
    assert_ok(&run(&daemon, &["action", "focus", "h"]));
    assert_ok(&run(&daemon, &["action", "move", "j"]));
    assert_eq!(
        daemon.request_values(),
        vec![
            json!({"request": "action", "data": {"action": "focus", "args": {"direction": "left"}}}),
            json!({"request": "action", "data": {"action": "move", "args": {"direction": "down"}}}),
        ]
    );
}

#[test]
fn action_raw_json_is_forwarded() {
    let daemon = mock(vec![ok_reply()]);
    let output = run(
        &daemon,
        &[
            "action",
            "--json",
            r#"{"action":"focus","args":{"direction":"left"}}"#,
        ],
    );
    assert_ok(&output);
    assert_eq!(
        daemon.requests(),
        vec![r#"{"request":"action","data":{"action":"focus","args":{"direction":"left"}}}"#]
    );
}

#[test]
fn action_json_output_prints_ok() {
    let daemon = mock(vec![ok_reply()]);
    let output = run(&daemon, &["action", "close", "--json"]);
    assert_ok(&output);
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value, json!({"ok": true}));
}

#[test]
fn action_usage_errors() {
    for args in [
        vec!["action"],
        vec!["action", "frobnicate"],
        vec!["action", "focus"],
        vec!["action", "focus", "sideways"],
        vec!["action", "workspace", "0"],
        vec!["action", "move-to-workspace", "nope"],
        vec!["action", "close", "extra"],
        vec!["action", "--json", "{}"],
    ] {
        let output = cli().args(&args).output().unwrap();
        assert_exit(&output, 2);
        assert!(
            !stderr(&output).is_empty(),
            "expected an error message for {args:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// config, restore, ping
// ---------------------------------------------------------------------------------------------

#[test]
fn config_renders_human_readable_values() {
    let daemon = mock(vec![config_reply()]);
    let output = run(&daemon, &["config"]);
    assert_ok(&output);
    let text = stdout(&output);
    assert!(text.contains("gaps: outer=8 inner=4"));
    assert!(text.contains("active_hint: 2"));
    assert!(text.contains("autotile: true"));
    assert!(text.contains("app_id=com.apple.finder"));
    assert!(text.contains("com.apple.Terminal"));
    assert_request(&daemon, json!({"request": "config"}));
}

#[test]
fn config_json_is_the_protocol_payload() {
    let daemon = mock(vec![config_reply()]);
    let output = run(&daemon, &["config", "--json"]);
    assert_ok(&output);
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value, config_payload());
}

#[test]
fn restore_sends_the_restore_request() {
    let daemon = mock(vec![ok_reply()]);
    let output = run(&daemon, &["restore"]);
    assert_ok(&output);
    assert!(stdout(&output).contains("restore requested"));
    assert_request(&daemon, json!({"request": "restore"}));
}

#[test]
fn ping_uses_list_windows_without_a_protocol_ping() {
    let daemon = mock(vec![windows_reply()]);
    let output = run(&daemon, &["ping"]);
    assert_ok(&output);
    assert!(stdout(&output).contains("2 windows"));
    assert_request(&daemon, json!({"request": "list_windows"}));
}

// ---------------------------------------------------------------------------------------------
// Daemon errors and IPC failures
// ---------------------------------------------------------------------------------------------

#[test]
fn daemon_error_exits_one() {
    let daemon = mock(vec![error_reply("no focused window")]);
    let output = run(&daemon, &["action", "close"]);
    assert_exit(&output, 1);
    assert!(stderr(&output).contains("daemon error: no focused window"));
}

#[test]
fn daemon_error_on_query_exits_one() {
    let daemon = mock(vec![error_reply("boom")]);
    let output = run(&daemon, &["list-windows"]);
    assert_exit(&output, 1);
    assert!(stderr(&output).contains("daemon error: boom"));
}

#[test]
fn malformed_reply_exits_three() {
    let daemon = mock(vec!["this is not json".to_string()]);
    let output = run(&daemon, &["list-windows"]);
    assert_exit(&output, 3);
    assert!(stderr(&output).contains("invalid reply"));
    assert!(stderr(&output).contains("this is not json"));
}

#[test]
fn eof_without_reply_exits_three() {
    let daemon = mock(vec![String::new()]);
    let output = run(&daemon, &["list-windows"]);
    assert_exit(&output, 3);
    assert!(stderr(&output).contains("closed the connection"));
}

#[test]
fn timeout_exits_three() {
    let daemon = MockDaemon::with_delay(vec![ok_reply()], std::time::Duration::from_millis(300));
    let mut command = cli();
    command
        .arg("--socket")
        .arg(daemon.path())
        .args(["--timeout-ms", "50", "list-windows"]);
    let output = command.output().unwrap();
    assert_exit(&output, 3);
    assert!(stderr(&output).contains("timed out after 50 ms"));
}

#[test]
fn missing_socket_exits_three_with_a_start_hint() {
    let dir = TempDir::new("cosmic-mac-ctl-missing");
    let path = dir.path().join("nope.sock");
    let output = run_at(&path, &["list-windows"]);
    assert_exit(&output, 3);
    let text = stderr(&output);
    assert!(text.contains("daemon not running: no socket at"));
    assert!(text.contains("cosmic-mac-wm"));
}

#[test]
fn refused_socket_exits_three_with_a_start_hint() {
    let dir = TempDir::new("cosmic-mac-ctl-refused");
    let path = dir.path().join("stale.sock");
    {
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
    }
    let output = run_at(&path, &["list-windows"]);
    assert_exit(&output, 3);
    let text = stderr(&output);
    assert!(text.contains("nothing is listening"), "stderr:\n{text}");
    assert!(text.contains("cosmic-mac-wm"));
}

#[test]
fn unexpected_response_variant_exits_three() {
    let daemon = mock(vec![tree_reply()]);
    let output = run(&daemon, &["list-windows"]);
    assert_exit(&output, 3);
    assert!(stderr(&output).contains("expected `windows`, got `tree`"));
}

// ---------------------------------------------------------------------------------------------
// Socket path resolution
// ---------------------------------------------------------------------------------------------

#[test]
fn socket_environment_variable_is_used() {
    let daemon = mock(vec![ok_reply()]);
    let output = run_env(&daemon, &["action", "close"]);
    assert_ok(&output);
    assert_eq!(daemon.requests().len(), 1);
}

#[test]
fn socket_flag_overrides_the_environment_variable() {
    let flag_daemon = mock(vec![ok_reply()]);
    let env_daemon = mock(vec![ok_reply()]);
    let output = cli()
        .env("COSMIC_MAC_WM_SOCKET", env_daemon.path())
        .arg("--socket")
        .arg(flag_daemon.path())
        .args(["action", "close"])
        .output()
        .unwrap();
    assert_ok(&output);
    assert_eq!(flag_daemon.requests().len(), 1);
    assert!(env_daemon.requests().is_empty());
}

#[test]
fn default_socket_lives_under_home_not_tmpdir() {
    let home = TempDir::new("cosmic-mac-ctl-home");
    let output = cli()
        .env("HOME", home.path())
        .env_remove("COSMIC_MAC_WM_SOCKET")
        .arg("list-windows")
        .output()
        .unwrap();
    assert_exit(&output, 3);
    let text = stderr(&output);
    assert!(
        text.contains("Library/Application Support/cosmic/cosmic-mac-wm.sock"),
        "stderr:\n{text}"
    );
}
