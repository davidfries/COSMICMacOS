#!/usr/bin/env bash
# vm-test.sh — drive cosmic-mac-ctl against the real daemon in the UTM macOS VM.
#
# The scenario builds and deploys cosmic-mac-ctl (and cosmic-mac-wm when its crate
# exists), opens real windows, drives every interesting action through the CLI and
# asserts the resulting geometry from `list-windows --json`. Screenshots are copied
# back into scratch/. It ends with Restore + SIGTERM and checks that the windows are
# back at their pre-manager frames.
#
# Usage:
#   scripts/vm-test.sh                     # full run
#   scripts/vm-test.sh --no-vm             # dry run: print every step, touch nothing
#   scripts/vm-test.sh --windows 4         # open 4 test windows (default 3)
#   scripts/vm-test.sh --no-build          # deploy existing target/release binaries
#   scripts/vm-test.sh --no-screenshot     # skip screencapture/scp
#   scripts/vm-test.sh --keep-daemon       # leave a daemon we started running
#   scripts/vm-test.sh --lock-file /tmp/x  # override the concurrency lock
#
# Concurrency: a lock directory (<lock>.d) is taken for the whole run. If another
# vm-test (or the daemon owner's smoke test, if it honours the lock) holds it, this
# script prints a notice and exits 0 without touching the VM.
#
# Requirements on the host: ssh BatchMode access to $VM_HOST, rsync, cargo.
# Requirements in the VM: python3 (ships with the developer tools), Accessibility
# granted to the daemon, and Screen Recording for screenshots (best effort).
#
# Exit status: 0 success or lock-skip, nonzero on the first failed assertion.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM_HOST="${VM_HOST:-dj@192.168.64.14}"
REMOTE_BIN="${REMOTE_BIN:-cosmicMac/bin}"

WINDOWS=3
DRY=0
BUILD=1
SCREENSHOTS=1
KEEP_DAEMON=0
LOCK_FILE="${LOCK_FILE:-/tmp/cosmicmac-vm-test.lock}"

SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10)
HELPER=/tmp/cosmicmac-vm-helpers.sh
ASSERT=/tmp/cosmicmac-vm-assert.py
BASELINE=/tmp/cosmicmac-vm-baseline.json
BEFORE=/tmp/cosmicmac-vm-before.json
AFTER=/tmp/cosmicmac-vm-after.json

# Shell fragment executed remotely. $HOME must be expanded by the remote shell.
REMOTE_CTL="\"\$HOME/$REMOTE_BIN/cosmic-mac-ctl\" --socket \"\$HOME/Library/Application Support/cosmic/cosmic-mac-wm.sock\""

STARTED_DAEMON=0
DAEMON_STOPPED=0
LOCK_HELD=0
COUNT="$WINDOWS"

usage() {
    sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-vm|--dry-run)
            DRY=1
            shift
            ;;
        --windows)
            [[ $# -ge 2 ]] || { echo "--windows requires a number" >&2; exit 64; }
            WINDOWS="$2"
            shift 2
            ;;
        --no-build)
            BUILD=0
            shift
            ;;
        --no-screenshot)
            SCREENSHOTS=0
            shift
            ;;
        --keep-daemon)
            KEEP_DAEMON=1
            shift
            ;;
        --lock-file)
            [[ $# -ge 2 ]] || { echo "--lock-file requires a path" >&2; exit 64; }
            LOCK_FILE="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 64
            ;;
    esac
done

if ! [[ "$WINDOWS" =~ ^[0-9]+$ ]] || [[ "$WINDOWS" -lt 1 ]]; then
    echo "--windows must be a positive integer" >&2
    exit 64
fi
COUNT="$WINDOWS"

LOCK_DIR="${LOCK_FILE}.d"

step() { printf '\n== %s\n' "$*"; }
warn() { printf '!! %s\n' "$*" >&2; }
die() { printf '!! FAILED: %s\n' "$*" >&2; exit 1; }

dry() { [[ "$DRY" -eq 1 ]]; }

# Run a command on the VM, aborting on failure. In dry-run mode, print it instead.
run_remote() {
    local desc="$1"
    local cmd="$2"
    if dry; then
        printf '[dry-run] ssh %s %s\n' "$VM_HOST" "$cmd"
        return 0
    fi
    step "$desc"
    if ! ssh "${SSH_OPTS[@]}" "$VM_HOST" "$cmd"; then
        die "$desc"
    fi
}

# Run a command on the VM and return its status without aborting.
remote_ok() {
    local desc="$1"
    local cmd="$2"
    if dry; then
        printf '[dry-run] check: ssh %s %s\n' "$VM_HOST" "$cmd"
        return 0
    fi
    ssh "${SSH_OPTS[@]}" "$VM_HOST" "$cmd" >/dev/null 2>&1
}

# Run a command on the VM and print its stdout. `fallback` is used by dry runs.
remote_capture() {
    local desc="$1"
    local cmd="$2"
    local fallback="${3:-}"
    if dry; then
        printf '[dry-run] capture (%s): ssh %s %s\n' "$desc" "$VM_HOST" "$cmd" >&2
        printf '%s\n' "$fallback"
        return 0
    fi
    printf '\n== %s\n' "$desc" >&2
    ssh "${SSH_OPTS[@]}" "$VM_HOST" "$cmd"
}

# Assert a remote command succeeds; a failure names the assertion and stops the run.
remote_assert() {
    local desc="$1"
    local cmd="$2"
    if dry; then
        printf '[dry-run] assert %s: ssh %s %s\n' "$desc" "$VM_HOST" "$cmd"
        return 0
    fi
    step "assert: $desc"
    if ! ssh "${SSH_OPTS[@]}" "$VM_HOST" "$cmd"; then
        die "assertion failed: $desc"
    fi
}

screenshot() {
    local name="$1"
    [[ "$SCREENSHOTS" -eq 1 ]] || return 0
    if dry; then
        printf '[dry-run] %s/scripts/screenshot.sh --name %s\n' "$ROOT" "$name"
        return 0
    fi
    step "screenshot: $name"
    "$ROOT/scripts/screenshot.sh" --name "$name" \
        || warn "screenshot failed (is Screen Recording granted to sshd?)"
}

# ---------------------------------------------------------------------------------------------
# Lock
# ---------------------------------------------------------------------------------------------

acquire_lock() {
    dry && return 0
    if mkdir "$LOCK_DIR" 2>/dev/null; then
        printf '%s\n' "$$" > "$LOCK_DIR/pid"
        LOCK_HELD=1
        return 0
    fi

    local holder=""
    [[ -f "$LOCK_DIR/pid" ]] && holder="$(cat "$LOCK_DIR/pid" 2>/dev/null || true)"
    if [[ -n "$holder" ]] && kill -0 "$holder" 2>/dev/null; then
        echo "another vm-test run (pid $holder) holds $LOCK_DIR; skipping gracefully"
        exit 0
    fi

    warn "removing stale lock $LOCK_DIR (holder ${holder:-unknown} is gone)"
    rm -rf "$LOCK_DIR"
    if mkdir "$LOCK_DIR" 2>/dev/null; then
        printf '%s\n' "$$" > "$LOCK_DIR/pid"
        LOCK_HELD=1
    else
        echo "could not acquire $LOCK_DIR; another run started concurrently; skipping"
        exit 0
    fi
}

release_lock() {
    if [[ "$LOCK_HELD" -eq 1 ]]; then
        rm -rf "$LOCK_DIR"
        LOCK_HELD=0
    fi
}

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if ! dry; then
        if [[ "$STARTED_DAEMON" -eq 1 && "$DAEMON_STOPPED" -eq 0 && "$KEEP_DAEMON" -eq 0 ]]; then
            warn "cleanup: restoring windows and stopping the daemon this run started"
            ssh "${SSH_OPTS[@]}" "$VM_HOST" "$REMOTE_CTL restore" >/dev/null 2>&1 || true
            ssh "${SSH_OPTS[@]}" "$VM_HOST" "bash $HELPER stop-daemon" >/dev/null 2>&1 || true
        fi
        release_lock
    fi
    exit "$status"
}

trap cleanup EXIT INT TERM
acquire_lock

# ---------------------------------------------------------------------------------------------
# Build + deploy
# ---------------------------------------------------------------------------------------------

build_and_deploy() {
    if dry; then
        printf '[dry-run] cd %s && cargo build --release -p cosmic-mac-ctl\n' "$ROOT"
        printf '[dry-run] if crates/cosmic-mac-wm/Cargo.toml exists: cargo build --release -p cosmic-mac-wm\n'
        printf '[dry-run] %s/scripts/sign.sh --verify <binaries> (best effort)\n' "$ROOT"
        printf '[dry-run] ssh %s mkdir -p ~/%s && rsync -az <binaries> %s:~/%s/\n' \
            "$VM_HOST" "$REMOTE_BIN" "$VM_HOST" "$REMOTE_BIN"
        return 0
    fi

    local binaries=()
    if [[ "$BUILD" -eq 1 ]]; then
        step "build cosmic-mac-ctl (host)"
        (cd "$ROOT" && cargo build --release -p cosmic-mac-ctl)
    fi

    local ctl="$ROOT/target/release/cosmic-mac-ctl"
    [[ -x "$ctl" ]] || die "missing $ctl (run without --no-build)"
    binaries+=("$ctl")

    local daemon="$ROOT/target/release/cosmic-mac-wm"
    if [[ -f "$ROOT/crates/cosmic-mac-wm/Cargo.toml" ]]; then
        if [[ "$BUILD" -eq 1 ]]; then
            step "build cosmic-mac-wm (host, optional)"
            if (cd "$ROOT" && cargo build --release -p cosmic-mac-wm); then
                [[ -x "$daemon" ]] && binaries+=("$daemon")
            else
                warn "cosmic-mac-wm did not build; continuing with the CLI only"
            fi
        elif [[ -x "$daemon" ]]; then
            binaries+=("$daemon")
        fi
    fi

    step "sign deployed binaries (best effort)"
    "$ROOT/scripts/sign.sh" --verify "${binaries[@]}" \
        || warn "signing failed; Accessibility may be lost after this deploy"

    step "deploy to $VM_HOST:~/$REMOTE_BIN"
    ssh "${SSH_OPTS[@]}" "$VM_HOST" "mkdir -p \"\$HOME/$REMOTE_BIN\""
    rsync -az "${binaries[@]}" "$VM_HOST:$REMOTE_BIN/"
    ssh "${SSH_OPTS[@]}" "$VM_HOST" "chmod +x \"\$HOME/$REMOTE_BIN\"/*"
}

# ---------------------------------------------------------------------------------------------
# Remote helpers (deployed once per run)
# ---------------------------------------------------------------------------------------------

deploy_helpers() {
    if dry; then
        printf '[dry-run] upload remote helper to %s:%s\n' "$VM_HOST" "$HELPER"
        printf '[dry-run] upload geometry assertions to %s:%s\n' "$VM_HOST" "$ASSERT"
        return 0
    fi

    step "deploy remote helpers"
    ssh "${SSH_OPTS[@]}" "$VM_HOST" "cat > '$HELPER'" <<'HELPER_EOF'
#!/usr/bin/env bash
# Deployed by scripts/vm-test.sh. Do not edit on the VM.
set -euo pipefail

REMOTE_BIN="${REMOTE_BIN:-cosmicMac/bin}"
BIN="$HOME/$REMOTE_BIN"
CTL="$BIN/cosmic-mac-ctl"
SOCK="$HOME/Library/Application Support/cosmic/cosmic-mac-wm.sock"
PIDFILE=/tmp/cosmicmac-wm.pid
LOG=/tmp/cosmicmac-wm.log
BASELINE=/tmp/cosmicmac-vm-baseline.json
AFTER=/tmp/cosmicmac-vm-after.json
PREFIX="cosmicmac-vm-test"

ctl() { "$CTL" --socket "$SOCK" "$@"; }

count_test_windows() {
    ctl list-windows --json 2>/dev/null \
        | python3 /tmp/cosmicmac-vm-assert.py count-test 2>/dev/null \
        || echo 0
}

close_test_windows() {
    # Prefer the daemon (it knows every window); fall back to AppleScript when it is down.
    if ctl ping >/dev/null 2>&1; then
        local wids
        wids="$(ctl list-windows --json 2>/dev/null \
            | python3 /tmp/cosmicmac-vm-assert.py test-wids 2>/dev/null || true)"
        for wid in $wids; do
            ctl focus "$wid" >/dev/null 2>&1 || true
            ctl action close >/dev/null 2>&1 || true
        done
        sleep 1
    fi

    osascript >/dev/null 2>&1 <<'APPLESCRIPT' || true
tell application "TextEdit"
    repeat with d in (every document whose name starts with "cosmicmac-vm-test")
        try
            close d saving no
        end try
    end repeat
end tell
tell application "Terminal"
    repeat with w in (every window)
        try
            if name of w contains "cosmicmac-vm-test" then close w
        end try
    end repeat
end tell
APPLESCRIPT
}

open_windows() {
    local want="$1"
    close_test_windows || true

    # TextEdit first: open the app, then one new instance per file. New instances are
    # what makes TextEdit show multiple independent windows over SSH.
    open -a TextEdit >/dev/null 2>&1 || true
    local i
    for i in $(seq 1 "$want"); do
        printf 'cosmicMac vm test window %s\n' "$i" > "/tmp/$PREFIX-$i.txt"
        open -n -a TextEdit "/tmp/$PREFIX-$i.txt" >/dev/null 2>&1 || true
    done
    sleep 2

    local have
    have="$(count_test_windows)"
    if (( have < want )); then
        # Terminal fallback: one window per `do script`, with a matching custom title.
        close_test_windows || true
        for i in $(seq 1 "$want"); do
            osascript \
                -e 'tell application "Terminal"' \
                -e 'do script ""' \
                -e "set custom title of front window to \"$PREFIX-$i\"" \
                -e 'end tell' >/dev/null 2>&1 || true
        done
        sleep 2
    fi

    local deadline=$((SECONDS + 30))
    while (( SECONDS < deadline )); do
        have="$(count_test_windows)"
        if (( have >= want )); then
            echo "$have"
            return 0
        fi
        sleep 0.5
    done
    echo "opened only $have of $want test windows" >&2
    ctl list-windows >&2 || true
    return 1
}

case "${1:-}" in
    ping)
        ctl ping
        ;;
    count-test-windows)
        count_test_windows
        ;;
    test-wids)
        ctl list-windows --json | python3 /tmp/cosmicmac-vm-assert.py test-wids
        ;;
    open-windows)
        open_windows "${2:?open-windows needs a count}"
        ;;
    close-test-windows)
        close_test_windows
        ;;
    start-daemon)
        if [[ ! -x "$BIN/cosmic-mac-wm" ]]; then
            echo "no daemon binary at $BIN/cosmic-mac-wm" >&2
            exit 1
        fi
        if [[ -f "$PIDFILE" ]] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
            echo "already running (pid $(cat "$PIDFILE"))"
            exit 0
        fi
        nohup "$BIN/cosmic-mac-wm" >"$LOG" 2>&1 &
        echo $! > "$PIDFILE"
        echo "started pid $(cat "$PIDFILE")"
        ;;
    wait-daemon)
        deadline=$((SECONDS + 20))
        while (( SECONDS < deadline )); do
            if ctl ping >/dev/null 2>&1; then
                echo "daemon ready"
                exit 0
            fi
            sleep 0.25
        done
        echo "daemon did not become ready; last log lines:" >&2
        tail -40 "$LOG" >&2 || true
        exit 1
        ;;
    daemon-pid)
        if [[ -f "$PIDFILE" ]] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
            cat "$PIDFILE"
            exit 0
        fi
        pgrep -f "$BIN/cosmic-mac-wm" | head -1
        ;;
    stop-daemon)
        pid="$("$0" daemon-pid 2>/dev/null || true)"
        if [[ -z "$pid" ]]; then
            echo "daemon not running"
            exit 0
        fi
        kill -TERM "$pid" 2>/dev/null || true
        for _ in $(seq 1 40); do
            if ! kill -0 "$pid" 2>/dev/null; then
                rm -f "$PIDFILE"
                echo "daemon stopped"
                exit 0
            fi
            sleep 0.25
        done
        echo "daemon $pid did not exit after SIGTERM" >&2
        tail -40 "$LOG" >&2 || true
        exit 1
        ;;
    baseline)
        ctl list-windows --json > "$BASELINE"
        echo "$BASELINE"
        ;;
    frames)
        out="$(osascript <<'APPLESCRIPT'
set output to ""
tell application "System Events"
    repeat with appName in {"TextEdit", "Terminal"}
        if exists process appName then
            tell process appName
                repeat with w in windows
                    try
                        set p to position of w
                        set s to size of w
                        set output to output & (name of w) & tab & (item 1 of p) & tab & (item 2 of p) & tab & (item 1 of s) & tab & (item 2 of s) & linefeed
                    end try
                end repeat
            end tell
        end if
    end repeat
end tell
return output
APPLESCRIPT
)"
        if [[ -z "$out" ]]; then
            echo "no AX frames available (System Events Accessibility not granted?)" >&2
            exit 1
        fi
        printf '%s' "$out"
        ;;
    *)
        echo "unknown helper command: ${1:-}" >&2
        exit 2
        ;;
esac
HELPER_EOF

    ssh "${SSH_OPTS[@]}" "$VM_HOST" "cat > '$ASSERT'" <<'PYTHON_EOF'
#!/usr/bin/env python3
"""Geometry assertions for scripts/vm-test.sh.

Reads `cosmic-mac-ctl list-windows --json` (or list-workspaces) on stdin.
Test windows are recognised by their title prefix, so unrelated windows on the
VM desktop do not break the scenario.
"""
import hashlib
import json
import sys

PREFIX = "cosmicmac-vm-test"
TOL = 2.0


def fail(message):
    print("ASSERT FAILED: " + message, file=sys.stderr)
    sys.exit(1)


def read(path=None):
    try:
        if path:
            with open(path, "r", encoding="utf-8") as handle:
                return json.load(handle)
        return json.load(sys.stdin)
    except Exception as error:  # noqa: BLE001 - CLI boundary
        fail("cannot read JSON: %s" % error)


def test_windows(windows):
    return [
        window
        for window in windows
        if window.get("info", {}).get("title", "").startswith(PREFIX)
    ]


def frame(window):
    rect = window["frame"]
    return (
        float(rect["x"]),
        float(rect["y"]),
        float(rect["width"]),
        float(rect["height"]),
    )


def close(a, b, tol=TOL):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def overlap(a, b):
    ax, ay, aw, ah = a
    bx, by, bw, bh = b
    ox = min(ax + aw, bx + bw) - max(ax, bx)
    oy = min(ay + ah, by + bh) - max(ay, by)
    return max(0.0, ox) * max(0.0, oy)


def bbox(frames):
    x = min(item[0] for item in frames)
    y = min(item[1] for item in frames)
    right = max(item[0] + item[2] for item in frames)
    bottom = max(item[1] + item[3] for item in frames)
    return (x, y, right - x, bottom - y)


def visible_tiled(windows):
    return [
        window
        for window in test_windows(windows)
        if window.get("visible") and not window.get("floating")
    ]


def mode_tiled(windows, want):
    tiles = visible_tiled(windows)
    if len(tiles) != want:
        fail("expected %d visible tiled test windows, found %d" % (want, len(tiles)))
    frames = [frame(window) for window in tiles]
    for index, first in enumerate(frames):
        for second in frames[index + 1 :]:
            if overlap(first, second) > 1.0:
                fail("tiled frames overlap: %s and %s" % (first, second))
    area = bbox(frames)
    for item in frames:
        if (
            item[0] < area[0] - TOL
            or item[1] < area[1] - TOL
            or item[0] + item[2] > area[0] + area[2] + TOL
            or item[1] + item[3] > area[1] + area[3] + TOL
        ):
            fail("frame %s escapes the tiling area %s" % (item, area))
        if item[2] < 50 or item[3] < 50:
            fail("frame %s is too small to be a tile" % (item,))
    all_visible = [
        window
        for window in windows
        if window.get("visible") and not window.get("floating")
    ]
    if len(all_visible) == len(tiles):
        covered = sum(item[2] * item[3] for item in frames)
        total = area[2] * area[3]
        if total > 0 and covered / total < 0.90:
            fail(
                "tiles cover only %.1f%% of their bounding box (gaps larger than expected?)"
                % (100.0 * covered / total)
            )


def mode_focus(windows, side):
    tiles = visible_tiled(windows)
    if not tiles:
        fail("no visible tiled test windows")
    if side == "leftmost":
        target = min(tiles, key=lambda window: (frame(window)[0], frame(window)[1]))
    else:
        target = max(
            tiles,
            key=lambda window: (frame(window)[0] + frame(window)[2], frame(window)[1]),
        )
    focused = [window for window in tiles if window.get("focused")]
    if len(focused) != 1:
        fail("expected exactly one focused test window, found %d" % len(focused))
    if focused[0]["info"]["wid"] != target["info"]["wid"]:
        fail(
            "focused wid %s is not the %s window (wid %s)"
            % (focused[0]["info"]["wid"], side, target["info"]["wid"])
        )


def main():
    if len(sys.argv) < 2:
        fail("missing assertion mode")
    mode = sys.argv[1]

    if mode == "count-test":
        print(len(test_windows(read())))
        return

    if mode == "test-wids":
        print(" ".join(str(window["info"]["wid"]) for window in test_windows(read())))
        return

    if mode == "tiled":
        mode_tiled(read(), int(sys.argv[2]))
        return

    if mode in ("focus-leftmost", "focus-rightmost"):
        mode_focus(read(), mode.split("-", 1)[1])
        return

    if mode == "stacked":
        windows = test_windows(read())
        stacked = [window for window in windows if window.get("in_stack")]
        if len(stacked) < 2:
            fail("expected at least two stacked test windows, found %d" % len(stacked))
        active = [window for window in stacked if window.get("stack_active")]
        if len(active) != 1:
            fail("expected exactly one active stack member, found %d" % len(active))
        visible = [window for window in stacked if window.get("visible")]
        if len(visible) != 1:
            fail("expected exactly one visible stack member, found %d" % len(visible))
        return

    if mode == "unstacked":
        stacked = [window for window in test_windows(read()) if window.get("in_stack")]
        if stacked:
            fail("expected no stacked test windows, found %d" % len(stacked))
        return

    if mode == "visible":
        want = int(sys.argv[2])
        visible = [
            window for window in test_windows(read()) if window.get("visible")
        ]
        if len(visible) != want:
            fail("expected %d visible test windows, found %d" % (want, len(visible)))
        return

    if mode == "floating":
        want = int(sys.argv[2])
        floating = [
            window for window in test_windows(read()) if window.get("floating")
        ]
        if len(floating) != want:
            fail("expected %d floating test windows, found %d" % (want, len(floating)))
        return

    if mode == "count":
        want = int(sys.argv[2])
        total = len(test_windows(read()))
        if total != want:
            fail("expected %d test windows in total, found %d" % (want, total))
        return

    if mode == "workspace":
        want = int(sys.argv[2])
        workspaces = read()
        active = [workspace for workspace in workspaces if workspace.get("active")]
        if len(active) != 1:
            fail("expected exactly one active workspace, found %d" % len(active))
        if active[0]["index"] != want:
            fail("expected active workspace %d, got %d" % (want, active[0]["index"]))
        return

    if mode == "maximized":
        tiles = visible_tiled(read())
        if len(tiles) < 2:
            fail("maximize needs at least two tiled windows, found %d" % len(tiles))
        frames = [frame(window) for window in tiles]
        area = bbox(frames)
        matching = [
            window for window in tiles if close(frame(window), area, TOL)
        ]
        if len(matching) != 1:
            fail(
                "expected exactly one maximized window matching %s, found %d"
                % (area, len(matching))
            )
        return

    if mode == "geometry-changed":
        before = read(sys.argv[2])
        after = read()
        before_frames = {
            window["info"]["wid"]: frame(window) for window in test_windows(before)
        }
        compared = 0
        for window in test_windows(after):
            wid = window["info"]["wid"]
            if wid not in before_frames:
                continue
            compared += 1
            if not close(before_frames[wid], frame(window)):
                return
        if compared == 0:
            fail("no common test windows to compare geometry")
        fail("geometry did not change")

    if mode == "hash":
        windows = test_windows(read())
        digest = hashlib.sha1()
        for window in sorted(windows, key=lambda item: item["info"]["wid"]):
            digest.update(
                ("%s:%s\n" % (window["info"]["wid"], frame(window))).encode("utf-8")
            )
        print(digest.hexdigest())
        return

    if mode == "restored":
        baseline = read(sys.argv[2])
        current = read()
        initial = {
            window["info"]["wid"]: window["info"].get("initial_frame")
            for window in test_windows(baseline)
        }
        compared = 0
        for window in test_windows(current):
            wid = window["info"]["wid"]
            target = initial.get(wid)
            if not target:
                continue
            compared += 1
            expected = (
                float(target["x"]),
                float(target["y"]),
                float(target["width"]),
                float(target["height"]),
            )
            if not close(frame(window), expected):
                fail(
                    "window %s frame %s differs from original %s"
                    % (wid, frame(window), expected)
                )
        if compared == 0:
            fail("baseline has no initial_frame values to compare")
        return

    if mode == "restored-ax":
        baseline = read(sys.argv[2])
        with open(sys.argv[3], "r", encoding="utf-8") as handle:
            lines = [line.rstrip("\n") for line in handle if line.strip()]
        ax_frames = []
        for line in lines:
            parts = line.split("\t")
            if len(parts) != 5:
                continue
            try:
                ax_frames.append(
                    (
                        parts[0],
                        (float(parts[1]), float(parts[2]), float(parts[3]), float(parts[4])),
                    )
                )
            except ValueError:
                continue
        compared = 0
        for window in test_windows(baseline):
            title = window["info"]["title"]
            target = window["info"].get("initial_frame")
            if not target:
                continue
            match = next(
                (frame_value for name, frame_value in ax_frames if name == title),
                None,
            )
            if match is None:
                match = next(
                    (
                        frame_value
                        for name, frame_value in ax_frames
                        if name.startswith(title) or title in name
                    ),
                    None,
                )
            if match is None:
                continue
            compared += 1
            expected = (
                float(target["x"]),
                float(target["y"]),
                float(target["width"]),
                float(target["height"]),
            )
            if not close(match, expected):
                fail(
                    "window %r frame %s differs from original %s"
                    % (title, match, expected)
                )
        if compared == 0:
            fail("could not match any restored window against the AX frame dump")
        return

    fail("unknown assertion mode %r" % mode)


if __name__ == "__main__":
    main()
PYTHON_EOF

    ssh "${SSH_OPTS[@]}" "$VM_HOST" "chmod +x '$HELPER'"
}

# ---------------------------------------------------------------------------------------------
# Scenario
# ---------------------------------------------------------------------------------------------

assert_windows() {
    local desc="$1"
    local assertion="$2"
    remote_assert "$desc" "$REMOTE_CTL list-windows --json | python3 $ASSERT $assertion"
}

assert_workspaces() {
    local desc="$1"
    local assertion="$2"
    remote_assert "$desc" "$REMOTE_CTL list-workspaces --json | python3 $ASSERT $assertion"
}

action() {
    run_remote "action: $*" "$REMOTE_CTL action $*"
}

snapshot_geometry() {
    run_remote "snapshot geometry to $BEFORE" "$REMOTE_CTL list-windows --json > $BEFORE"
}

build_and_deploy
deploy_helpers

step "preflight"
remote_assert "python3 is available in the VM" "command -v python3 >/dev/null"
remote_assert "cosmic-mac-ctl is deployed" "test -x \"\$HOME/$REMOTE_BIN/cosmic-mac-ctl\""

run_remote "close leftover test windows" "bash $HELPER close-test-windows"

if dry; then
    step "daemon state unknown (dry run)"
    printf '[dry-run] ping the daemon; if unreachable, start ~/%s/cosmic-mac-wm with nohup and wait for the socket\n' \
        "$REMOTE_BIN"
    printf '[dry-run] if this run starts the daemon, restore + SIGTERM + wait + post-exit frame check at the end\n'
elif remote_ok "daemon reachable" "$REMOTE_CTL ping"; then
    step "daemon already running; it will be left running at the end"
else
    if remote_ok "daemon binary present" "test -x \"\$HOME/$REMOTE_BIN/cosmic-mac-wm\""; then
        run_remote "start daemon (nohup)" "bash $HELPER start-daemon"
        STARTED_DAEMON=1
        run_remote "wait for the daemon socket" "bash $HELPER wait-daemon"
    else
        die "the daemon is not running and ~/$REMOTE_BIN/cosmic-mac-wm is not deployed"
    fi
fi

COUNT="$(remote_capture "open and wait for test windows" \
    "bash $HELPER open-windows $WINDOWS" "$WINDOWS")"
[[ -n "$COUNT" ]] || COUNT="$WINDOWS"
if ! dry && [[ "$COUNT" -lt "$WINDOWS" ]]; then
    die "expected at least $WINDOWS test windows, only $COUNT appeared"
fi
step "using $COUNT test window(s)"

run_remote "capture baseline frames" "bash $HELPER baseline"

assert_windows "windows tile the work area" "tiled $COUNT"
screenshot "vm-test-01-tiled"

action focus left
assert_windows "focus left picks the leftmost window" "focus-leftmost"

action focus right
assert_windows "focus right picks the rightmost window" "focus-rightmost"

action toggle-stacking
assert_windows "two windows form a stack with one active member" "stacked"
screenshot "vm-test-02-stack"

action toggle-stacking
assert_windows "toggling again unstacks" "unstacked"
assert_windows "windows are tiled again after unstacking" "tiled $COUNT"

action workspace 2
assert_workspaces "workspace 2 is active" "workspace 2"
assert_windows "workspace 2 shows no test windows" "visible 0"
screenshot "vm-test-03-workspace2"

action workspace 1
assert_workspaces "workspace 1 is active again" "workspace 1"
assert_windows "windows are back on workspace 1" "visible $COUNT"

action focus left
action move-to-workspace 2
assert_workspaces "moving a window does not follow it" "workspace 1"
assert_windows "one window left workspace 1" "visible $((COUNT - 1))"
assert_windows "the moved window still exists" "count $COUNT"

action next-workspace
assert_workspaces "next-workspace moved to workspace 2" "workspace 2"
assert_windows "the moved window is visible on workspace 2" "visible 1"

action previous-workspace
assert_workspaces "previous-workspace moved back to workspace 1" "workspace 1"

action last-workspace
assert_workspaces "last-workspace returns to workspace 2" "workspace 2"

action workspace 1
action focus left
action move-to-workspace 2
assert_workspaces "the second move stays on workspace 1" "workspace 1"
assert_windows "two windows left workspace 1" "visible $((COUNT - 2))"
assert_windows "all test windows still exist" "count $COUNT"

action workspace 2
assert_workspaces "workspace 2 is active" "workspace 2"
assert_windows "both moved windows are visible on workspace 2" "visible 2"

action move-to-workspace 1
assert_workspaces "moving one window back stays on workspace 2" "workspace 2"
assert_windows "one window remains on workspace 2" "visible 1"

action workspace 1
assert_windows "one window is still parked on workspace 2" "visible $((COUNT - 1))"

action workspace 2
action move-to-workspace 1
assert_workspaces "the empty trailing workspace 2 is removed" "workspace 1"
assert_windows "all windows are back on workspace 1" "visible $COUNT"

action toggle-tiling
assert_windows "toggle-tiling floats the whole workspace" "floating $COUNT"
assert_windows "floating windows stay visible" "visible $COUNT"
screenshot "vm-test-04-floating"

action toggle-tiling
assert_windows "toggle-tiling re-tiles the workspace" "tiled $COUNT"

action focus left
action toggle-window-floating
assert_windows "toggle-window-floating floats one window" "floating 1"
action toggle-window-floating
assert_windows "toggle-window-floating re-tiles it" "floating 0"
assert_windows "the workspace is tiled again" "tiled $COUNT"

action maximize
assert_windows "maximize gives one window the whole work area" "maximized"
screenshot "vm-test-05-maximized"
action maximize
assert_windows "unmaximize restores the tiling" "tiled $COUNT"

snapshot_geometry
action resize left
assert_windows "resize changes the focused window's frame" "geometry-changed $BEFORE"
assert_windows "the workspace still tiles after resize" "tiled $COUNT"

snapshot_geometry
action toggle-orientation
assert_windows "toggle-orientation changes the layout" "geometry-changed $BEFORE"
assert_windows "the workspace still tiles after orientation" "tiled $COUNT"

action swap-window
assert_windows "swap-window leaves a valid tiling" "tiled $COUNT"

action focus left
action close
COUNT=$((COUNT - 1))
assert_windows "close removed one test window" "count $COUNT"
assert_windows "the remaining windows still tile" "tiled $COUNT"

step "restore and shutdown"
run_remote "restore original frames" "$REMOTE_CTL restore"
screenshot "vm-test-06-restored"

if remote_ok "read AX frames after restore" "bash $HELPER frames > $AFTER"; then
    remote_assert "restored frames match the baseline (AX)" \
        "python3 $ASSERT restored-ax $BASELINE $AFTER"
else
    warn "System Events Accessibility is unavailable; falling back to daemon-reported frames"
    assert_windows "restored frames match the baseline (daemon)" "restored $BASELINE"
fi

if [[ "$STARTED_DAEMON" -eq 1 ]]; then
    if [[ "$KEEP_DAEMON" -eq 1 ]]; then
        warn "--keep-daemon: leaving the daemon this run started running"
    else
        run_remote "stop daemon with SIGTERM" "bash $HELPER stop-daemon"
        DAEMON_STOPPED=1
        remote_assert "daemon process is gone" "! bash $HELPER daemon-pid >/dev/null 2>&1"
        if remote_ok "read AX frames after daemon exit" "bash $HELPER frames > $AFTER"; then
            remote_assert "frames did not move when the daemon exited" \
                "python3 $ASSERT restored-ax $BASELINE $AFTER"
        else
            warn "skipping post-exit AX frame check (System Events Accessibility unavailable)"
        fi
    fi
else
    if ! dry; then
        step "daemon was already running before this test; leaving it running"
    fi
fi

if remote_ok "close test windows" "bash $HELPER close-test-windows"; then
    :
else
    warn "could not close the test windows; they are titled cosmicmac-vm-test-*"
fi

step "summary"
echo "scenario passed with $COUNT test window(s)"
echo "screenshots (if enabled): $ROOT/scratch/vm-test-*.png"
echo "daemon log: ssh $VM_HOST 'tail -50 /tmp/cosmicmac-wm.log'"
