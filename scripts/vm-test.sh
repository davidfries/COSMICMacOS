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
FOCUS_BEFORE=/tmp/cosmicmac-vm-focus-before.json
FOCUS_TREE=/tmp/cosmicmac-vm-focus-tree.json
AFTER=/tmp/cosmicmac-vm-after.json

# Shell fragment executed remotely. $HOME must be expanded by the remote shell.
REMOTE_CTL="\"\$HOME/$REMOTE_BIN/cosmic-mac-ctl\" --socket \"\$HOME/Library/Application Support/cosmic/cosmic-mac-wm.sock\""
# Synthetic key chords (Super = Command) and mouse drags for the keybinding and drag phases;
# see examples/send-key.rs.
REMOTE_SENDKEY="\"\$HOME/$REMOTE_BIN/send-key\""

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
            if (cd "$ROOT" && cargo build --release -p cosmic-mac-wm --bins --example send-key); then
                [[ -x "$daemon" ]] && binaries+=("$daemon")
            else
                warn "cosmic-mac-wm did not build; continuing with the CLI only"
            fi
        elif [[ -x "$daemon" ]]; then
            binaries+=("$daemon")
        fi
    fi

    # Synthetic keys and drags for the keybinding and drag phases (built with the daemon above).
    local sendkey="$ROOT/target/release/examples/send-key"
    [[ -x "$sendkey" ]] && binaries+=("$sendkey")

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
    # Kill every TextEdit instance first. This also removes stray "Untitled" documents,
    # which bare `open -a TextEdit` (no document) used to leave behind: an Untitled window
    # joins the tiling and breaks the scenario's focus assertions. The test documents live
    # in /tmp and are rewritten on every run, so killing TextEdit loses nothing.
    pkill -x TextEdit 2>/dev/null || true
    sleep 1

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

    # TextEdit instances are launched directly with a document each (`open -n`); a bare
    # `open -a TextEdit` would create an "Untitled" document window that then joins the
    # tiling and breaks the scenario's focus assertions.
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
    close-finder-windows)
        # Finder windows tile too, and where they land decides which window a
        # directional focus or Super+S picks; the assertions assume only test
        # windows are tiled.  Closed through the daemon (Accessibility): an
        # AppleScript `tell Finder` needs an Automation grant and times out.
        for wid in $(ctl list-windows --json | python3 -c 'import json,sys; print(" ".join(str(w["info"]["wid"]) for w in json.load(sys.stdin) if w["info"]["app_id"] == "com.apple.finder"))'); do
            ctl focus "$wid" >/dev/null 2>&1 && ctl action close >/dev/null 2>&1 || true
        done
        sleep 1
        ;;
    focus-visible-test)
        # Other apps' windows (a Finder window, say) may sit next to the test
        # windows in the tree, so "focus left" can land on them; focus a test
        # window explicitly before moving it.
        wid="$(ctl list-windows --json | python3 /tmp/cosmicmac-vm-assert.py visible-test-wid)"
        ctl focus "$wid"
        ;;
    open-windows)
        open_windows "${2:?open-windows needs a count}"
        ;;
    close-test-windows)
        close_test_windows
        ;;
    set-fullscreen)
        # set-fullscreen <pid> <title> <true|false>: native full screen through the
        # window's AXFullScreen attribute.  System Events needs no Automation grant for the
        # app (a `tell application "TextEdit"` does, and times out without one).
        osascript -e "tell application \"System Events\" to tell (first process whose unix id is $2) to set value of attribute \"AXFullScreen\" of (first window whose name contains \"$3\") to $4" >/dev/null
        ;;
    wake-display)
        # A sleeping virtual display enumerates as zero displays (CGGetActiveDisplayList),
        # and a daemon started then falls back to a bogus 1920x1080 work area.  Simulated
        # user activity wakes it; vm-prep's `pmset displaysleep 0` makes this unnecessary.
        caffeinate -u -t 3
        sleep 2
        ;;
    start-daemon)
        caffeinate -u -t 3
        sleep 2
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
        # The daemon waits for an active display before it binds the socket, so
        # keep the (virtual) display awake while waiting.
        deadline=$((SECONDS + 20))
        next_wake=0
        while (( SECONDS < deadline )); do
            if ctl ping >/dev/null 2>&1; then
                echo "daemon ready"
                exit 0
            fi
            if (( SECONDS >= next_wake )); then
                caffeinate -u -t 1 &
                next_wake=$((SECONDS + 4))
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
    -- Every process of the name: the scenario runs one TextEdit per document.
    repeat with appName in {"TextEdit", "Terminal"}
        repeat with proc in (every process whose name is (appName as text))
            tell proc
                repeat with w in windows
                    try
                        set p to position of w
                        set s to size of w
                        set output to output & (name of w) & tab & (item 1 of p) & tab & (item 2 of p) & tab & (item 1 of s) & tab & (item 2 of s) & linefeed
                    end try
                end repeat
            end tell
        end repeat
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


FOCUS_EPSILON = 0.5


def center(rect):
    return (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0)


def focus_sort_key(candidate):
    # Same total order as the layout's `compare_focus_candidates`: primary distance,
    # perpendicular distance, then topmost, then leftmost, then the lowest wid.
    wid, primary, perpendicular, y, x = candidate
    return (primary, perpendicular, y, x, wid)


def tree_contains(node, wid):
    if node["kind"] == "window":
        return node["wid"] == wid
    if node["kind"] == "stack":
        return any(tab["wid"] == wid for tab in node["tabs"])
    return any(tree_contains(child, wid) for child in node["children"])


def tree_path(node, wid, acc):
    """Path of nodes from `node` down to the leaf holding `wid`, or None."""
    acc.append(node)
    if tree_contains_leaf(node, wid):
        return acc
    if node["kind"] == "group":
        for child in node["children"]:
            found = tree_path(child, wid, acc)
            if found is not None:
                return found
    acc.pop()
    return None


def tree_contains_leaf(node, wid):
    if node["kind"] == "window":
        return node["wid"] == wid
    if node["kind"] == "stack":
        return any(tab["wid"] == wid for tab in node["tabs"])
    return False


def tree_descend_near(node, forward):
    while node["kind"] == "group":
        children = node["children"]
        node = children[0] if forward else children[-1]
    return node


def tree_descend_edge(node, forward):
    while node["kind"] == "group":
        children = node["children"]
        node = children[-1] if forward else children[0]
    return node


def tree_leaf_wid(node):
    if node["kind"] == "window":
        return node["wid"]
    if node["kind"] == "stack":
        return node["tabs"][node["active"]]["wid"]
    return None


ORIENTATIONS = {"left": "horizontal", "right": "horizontal",
                "up": "vertical", "down": "vertical"}
FORWARD = {"right": True, "down": True, "left": False, "up": False}
OPPOSITE = {"left": "right", "right": "left", "up": "down", "down": "up"}


def tree_focus(tree, from_wid, direction):
    """Port of `Tree::focus_neighbor` + `target_window` (cosmic-comp semantics).

    Walks the ancestors of `from_wid`'s leaf; at the first group whose orientation matches
    the direction's axis, the adjacent sibling is chosen and descended into on the near
    edge.  When no ancestor has a sibling, focus wraps to the opposite tree edge (only when
    the root splits along the same axis); otherwise the direction has no neighbour and the
    layout leaves focus where it is, so `from_wid` itself is returned.  Returns None only
    when `from_wid` is not in the tree (e.g. it floats) — the caller then falls back to the
    spatial oracle, exactly like `Layout::act_focus`.
    """
    if tree is None:
        return None
    path = tree_path(tree, from_wid, [])
    if path is None:
        return None
    forward = FORWARD[direction]
    handles = ORIENTATIONS[direction]
    # Ancestors from the leaf's parent up to the root.
    for index in range(len(path) - 2, -1, -1):
        group = path[index]
        if group["kind"] != "group" or group["orientation"] != handles:
            continue
        children = group["children"]
        position = next(
            i for i, child in enumerate(children) if child is path[index + 1]
        )
        target = position + 1 if forward else position - 1
        if 0 <= target < len(children):
            return tree_leaf_wid(tree_descend_near(children[target], forward))
    # No sibling at any level: wrap to the opposite edge of the root, but only when the
    # root splits along this axis.  Otherwise the screen edge is not a neighbour and the
    # focus does not move (a perpendicular direction, matching cosmic-comp).
    if tree["kind"] != "group" or tree["orientation"] != handles:
        return from_wid
    return tree_leaf_wid(tree_descend_edge(tree, FORWARD[OPPOSITE[direction]]))


def geometric_focus(tiles, from_wid, direction):
    """Independent re-implementation of the layout's directional-focus oracle.

    Operates on the pre-action snapshot so the assertion validates the focus SEMANTICS
    (direction, wrap-around, tie-break) rather than a specific tiling arrangement.
    """
    frames = {window["info"]["wid"]: frame(window) for window in tiles}
    if from_wid not in frames:
        return None
    cx, cy = center(frames[from_wid])
    candidates = [
        (wid,) + center(rect)
        for wid, rect in frames.items()
        if wid != from_wid
    ]
    if not candidates:
        return None

    # Directional pass: only windows strictly in the requested direction.
    best = None
    for wid, x, y in candidates:
        if direction == "left" and x < cx - FOCUS_EPSILON:
            key = (cx - x, abs(y - cy))
        elif direction == "right" and x > cx + FOCUS_EPSILON:
            key = (x - cx, abs(y - cy))
        elif direction == "up" and y < cy - FOCUS_EPSILON:
            key = (cy - y, abs(x - cx))
        elif direction == "down" and y > cy + FOCUS_EPSILON:
            key = (y - cy, abs(x - cx))
        else:
            continue
        candidate = (wid, key[0], key[1], y, x)
        if best is None or focus_sort_key(candidate) < focus_sort_key(best):
            best = candidate
    if best is not None:
        return best[0]

    # Wrap pass: nothing in that direction; the far edge on the opposite side wins.
    best = None
    for wid, x, y in candidates:
        if direction == "right":
            key = (x - cx, abs(y - cy))  # leftmost
        elif direction == "left":
            key = (cx - x, abs(y - cy))  # rightmost
        elif direction == "down":
            key = (y - cy, abs(x - cx))  # topmost
        else:
            key = (cy - y, abs(x - cx))  # bottommost
        candidate = (wid, key[0], key[1], y, x)
        if best is None or focus_sort_key(candidate) < focus_sort_key(best):
            best = candidate
    return best[0]


def mode_focus(windows, side, pre_path, tree_path):
    tiles = visible_tiled(windows)
    if not tiles:
        fail("no visible tiled test windows")
    pre_tiles = visible_tiled(read(pre_path))
    focused_before = [window for window in pre_tiles if window.get("focused")]
    if len(focused_before) != 1:
        fail(
            "pre-action snapshot has %d focused test windows, expected 1"
            % len(focused_before)
        )
    from_wid = focused_before[0]["info"]["wid"]
    direction = "left" if side == "leftmost" else "right"
    # Tiled focus is tree-based (cosmic-comp semantics); floating focus falls back to the
    # spatial search.  Try the tree first, matching `Layout::act_focus`.
    tree = read(tree_path)
    expected = tree_focus(tree, from_wid, direction)
    if expected is None:
        expected = geometric_focus(pre_tiles, from_wid, direction)
    if expected is None:
        fail("focus oracle found no target for %s from wid %s" % (direction, from_wid))
    focused = [window for window in tiles if window.get("focused")]
    if len(focused) != 1:
        fail("expected exactly one focused test window, found %d" % len(focused))
    actual = focused[0]["info"]["wid"]
    if actual != expected:
        fail(
            "focus %s from wid %s: focused wid %s, expected wid %s (oracle)"
            % (direction, from_wid, actual, expected)
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

    if mode == "visible-test-wid":
        visible = [window for window in test_windows(read()) if window.get("visible")]
        if not visible:
            fail("no visible test window")
        print(visible[0]["info"]["wid"])
        return

    if mode == "fullscreen-candidate":
        # "<pid>\t<title>" of a visible test window, preferring one without focus: a
        # window resumed next to the focused one instead of into its old tile then shows.
        visible = [window for window in test_windows(read()) if window.get("visible")]
        if not visible:
            fail("no visible test window")
        visible.sort(key=lambda window: bool(window.get("focused")))
        info = visible[0]["info"]
        print("%d\t%s" % (info["pid"], info["title"]))
        return

    if mode == "in-tiles":
        # Every visible test window is where the layout put it (ACTUAL = FRAME).
        for window in test_windows(read()):
            if not window.get("visible"):
                continue
            actual = window.get("actual_frame")
            if actual is None:
                fail("window %s has no actual frame" % window["info"]["wid"])
            rect = (
                float(actual["x"]),
                float(actual["y"]),
                float(actual["width"]),
                float(actual["height"]),
            )
            if not close(rect, frame(window)):
                fail(
                    "window %s is at %s, not in its tile %s"
                    % (window["info"]["wid"], rect, frame(window))
                )
        return

    if mode == "geometry-unchanged":
        before = {
            window["info"]["wid"]: frame(window) for window in test_windows(read(sys.argv[2]))
        }
        after = {window["info"]["wid"]: frame(window) for window in test_windows(read())}
        if sorted(before) != sorted(after):
            fail("test windows changed: %s before, %s now" % (sorted(before), sorted(after)))
        for wid, rect in before.items():
            if not close(rect, after[wid]):
                fail("window %s moved from %s to %s" % (wid, rect, after[wid]))
        return

    if mode == "tiled":
        mode_tiled(read(), int(sys.argv[2]))
        return

    if mode in ("focus-leftmost", "focus-rightmost"):
        if len(sys.argv) < 4:
            fail("focus assertions need the pre-action snapshot and tree paths")
        mode_focus(read(), mode.split("-", 1)[1], sys.argv[2], sys.argv[3])
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

    if mode in ("drag-to-centre", "drag-to-corner"):
        # "<wid> <target> X1,Y1 X2,Y2": a drag from one visible tiled test window's title
        # bar to another's tile, at its centre (in the middle third: stacks) or a sixth in
        # from its top-left corner (outside the middle third: snaps back).
        tiles = visible_tiled(read())
        if len(tiles) < 2:
            fail("a drag needs two visible tiled test windows, found %d" % len(tiles))
        source, target = tiles[0], tiles[1]
        actual = source.get("actual_frame")
        if actual is None:
            sx, sy, sw, _ = frame(source)
        else:
            sx, sy, sw = float(actual["x"]), float(actual["y"]), float(actual["width"])
        tx, ty, tw, th = frame(target)
        share = 2.0 if mode == "drag-to-centre" else 6.0
        print(
            "%d %d %.0f,%.0f %.0f,%.0f"
            % (
                source["info"]["wid"],
                target["info"]["wid"],
                sx + sw / 2.0,
                sy + 12.0,
                tx + tw / share,
                ty + th / share,
            )
        )
        return

    if mode == "stacked-onto":
        # stacked-onto <wid> <target>: both share one stack and <wid> is its visible tab.
        wid, target = int(sys.argv[2]), int(sys.argv[3])
        windows = {window["info"]["wid"]: window for window in test_windows(read())}
        dropped, under = windows.get(wid), windows.get(target)
        if dropped is None or under is None:
            fail("window %s or %s is gone" % (wid, target))
        if not (dropped.get("in_stack") and under.get("in_stack")):
            fail("windows %s and %s are not both stacked" % (wid, target))
        if not (dropped.get("stack_active") and dropped.get("visible")):
            fail("the dropped window %s is not the visible active tab" % wid)
        if under.get("visible"):
            fail("the target %s is still shown next to the dropped window" % target)
        if not close(frame(dropped), frame(under)):
            fail("windows %s and %s do not share a stack frame" % (wid, target))
        return

    if mode == "tab-point":
        # tab-point <wid> (reads `tree --json`): "X,Y" a third into <wid>'s tab in its
        # stack's bar, left of the close glyph (crates/cosmic-mac-wm/src/tabs.rs geometry).
        wid = int(sys.argv[2])
        stacks, nodes = [], [read()]
        while nodes:
            node = nodes.pop()
            if node.get("kind") == "stack":
                stacks.append(node)
            nodes.extend(node.get("children", []))
        for stack in stacks:
            wids = [tab["wid"] for tab in stack["tabs"]]
            if wid not in wids:
                continue
            x, y, width, _ = frame(stack)
            bar_width = max(width, 56.0)
            tab = min(max(bar_width / len(wids), 56.0), 220.0)
            print("%.0f,%.0f" % (x + wids.index(wid) * tab + tab / 3.0, y - 26.0 + 13.0))
            return
        fail("window %s is in no stack" % wid)

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

    if mode == "workspace-count":
        want = int(sys.argv[2])
        workspaces = read()
        if len(workspaces) != want:
            fail("expected %d workspaces, got %d" % (want, len(workspaces)))
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

# Post one key chord through the HID event stream and give the tap time to deliver it.
key() {
    run_remote "key: $*" "$REMOTE_SENDKEY $* && sleep 0.5"
}

# Drag with the left button from $1 to $2 ("X,Y" global points). The drag holds at its end
# point before the release, so every move reaches the daemon while the button is down.
drag() {
    run_remote "drag: $1 -> $2" \
        "$REMOTE_SENDKEY --drag $1 $2 --steps 20 --step-ms 16 --hold-ms 300 && sleep 1"
}

# Click the left button at $1 ("X,Y" global points), then let the daemon act on it.
click() {
    run_remote "click: $1" "$REMOTE_SENDKEY --click $1 && sleep 1.5"
}

snapshot_geometry() {
    run_remote "snapshot geometry to $BEFORE" "$REMOTE_CTL list-windows --json > $BEFORE"
}

snapshot_focus() {
    run_remote "snapshot pre-action focus to $FOCUS_BEFORE" \
        "$REMOTE_CTL list-windows --json > $FOCUS_BEFORE && $REMOTE_CTL tree --json > $FOCUS_TREE"
}

build_and_deploy
deploy_helpers

step "preflight"
remote_assert "python3 is available in the VM" "command -v python3 >/dev/null"
remote_assert "cosmic-mac-ctl is deployed" "test -x \"\$HOME/$REMOTE_BIN/cosmic-mac-ctl\""

run_remote "wake the VM display (a sleeping display enumerates as none)" "bash $HELPER wake-display"
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

run_remote "close Finder windows" "bash $HELPER close-finder-windows"

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

snapshot_focus
action focus left
assert_windows "focus left picks the window the oracle expects" "focus-leftmost $FOCUS_BEFORE $FOCUS_TREE"

snapshot_focus
action focus right
assert_windows "focus right picks the window the oracle expects" "focus-rightmost $FOCUS_BEFORE $FOCUS_TREE"

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

run_remote "focus a visible test window" "bash $HELPER focus-visible-test"
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
run_remote "focus a visible test window" "bash $HELPER focus-visible-test"
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
# Emptying the active workspace keeps the user on it (cosmic-comp semantics); the empty
# trailing workspace is trimmed only when the user leaves it.
assert_workspaces "emptying the active workspace 2 keeps it active" "workspace 2"
assert_windows "workspace 2 is empty after the move" "visible 0"
action workspace 1
assert_workspaces "leaving the empty workspace 2 trims it" "workspace-count 1"
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
# Resize works along the focused window's parent group axis (cosmic-comp semantics), so
# `resize left` is a no-op when that group is vertical; fall back to the vertical axis.
if remote_ok "probe: resize left changed geometry" \
    "$REMOTE_CTL list-windows --json | python3 $ASSERT geometry-changed $BEFORE"; then
    step "resize left changed the focused window's frame"
else
    snapshot_geometry
    action resize up
    assert_windows "resize (vertical) changes the focused window's frame" \
        "geometry-changed $BEFORE"
fi
assert_windows "the workspace still tiles after resize" "tiled $COUNT"

snapshot_geometry
action toggle-orientation
assert_windows "toggle-orientation changes the layout" "geometry-changed $BEFORE"
assert_windows "the workspace still tiles after orientation" "tiled $COUNT"

action swap-window
assert_windows "swap-window leaves a valid tiling" "tiled $COUNT"

# --- keybindings: the same actions driven through the CGEventTap -----------------------------
# The chords below are COSMIC defaults (Super = Command).  Each is verified by the effect the
# bound action has on the layout, so a chord that the tap does not see or swallow fails the
# assertion rather than silently passing through to the focused app.
if remote_ok "send-key is deployed" "test -x \"\$HOME/$REMOTE_BIN/send-key\""; then
    step "keybindings via the event tap"
    key Super+y
    assert_windows "Super+Y (ToggleTiling) floats the workspace" "floating $COUNT"
    key Super+y
    assert_windows "Super+Y again re-tiles the workspace" "tiled $COUNT"

    key Super+s
    assert_windows "Super+S (ToggleStacking) forms a stack" "stacked"
    screenshot "vm-test-07-key-stack"
    key Super+s
    assert_windows "Super+S again unstacks" "unstacked"
    assert_windows "windows tile again after the key-driven unstack" "tiled $COUNT"

    key Super+2
    assert_workspaces "Super+2 switches to workspace 2" "workspace 2"
    assert_windows "workspace 2 shows no test windows" "visible 0"
    key Super+1
    assert_workspaces "Super+1 switches back to workspace 1" "workspace 1"
    assert_windows "windows are visible again on workspace 1" "visible $COUNT"

    key Super+Shift+2
    assert_workspaces "Super+Shift+2 (MoveToWorkspace) does not follow the window" "workspace 1"
    assert_windows "Super+Shift+2 moved one window away" "visible $((COUNT - 1))"
    key Super+2
    key Super+Shift+1
    assert_workspaces "Super+Shift+1 empties workspace 2 but stays on it" "workspace 2"
    key Super+1
    assert_workspaces "Super+1 leaves and trims the empty workspace 2" "workspace-count 1"
    assert_windows "all windows are back on workspace 1" "visible $COUNT"

    snapshot_focus
    key Super+Left
    assert_windows "Super+Left (Focus) picks the window the oracle expects" \
        "focus-leftmost $FOCUS_BEFORE $FOCUS_TREE"
    snapshot_focus
    key Super+Right
    assert_windows "Super+Right (Focus) picks the window the oracle expects" \
        "focus-rightmost $FOCUS_BEFORE $FOCUS_TREE"

    key Super+m
    assert_windows "Super+M (Maximize) fills the work area" "maximized"
    key Super+m
    assert_windows "Super+M again restores the tiling" "tiled $COUNT"
    # Super+Q is deliberately not exercised here: if the tap missed it, Cmd+Q would reach
    # the single-document TextEdit instance and quit it, which looks identical to Close.
else
    warn "send-key is not deployed; skipping the keybinding phase"
fi

# --- drag to stack -----------------------------------------------------------------------------
# A title-bar drag that ends in the middle third of another tile stacks the dragged window onto
# that tile (COSMIC's stack zone); a drag that ends anywhere else snaps back into its tile.
if (( COUNT >= 2 )) && remote_ok "send-key is deployed" "test -x \"\$HOME/$REMOTE_BIN/send-key\""; then
    step "drag to stack"
    run_remote "wake the VM display (a sleeping display ignores synthetic clicks)" \
        "bash $HELPER wake-display"
    DRAG="$(remote_capture "pick a drag onto another tile's centre" \
        "$REMOTE_CTL list-windows --json | python3 $ASSERT drag-to-centre" "0 0 0,0 0,0")"
    read -r DRAG_WID DRAG_TARGET DRAG_FROM DRAG_TO <<<"$DRAG"
    drag "$DRAG_FROM" "$DRAG_TO"
    assert_windows "dropping on the tile's centre stacks $DRAG_WID onto $DRAG_TARGET" \
        "stacked-onto $DRAG_WID $DRAG_TARGET"
    assert_windows "the new stack tiles with the other windows" "tiled $((COUNT - 1))"
    screenshot "vm-test-09-drag-stack"
    # The stack's tab bar takes clicks: each tab shows (and focuses) its window.
    TAB="$(remote_capture "find the tab of $DRAG_TARGET in the stack's bar" \
        "$REMOTE_CTL tree --json | python3 $ASSERT tab-point $DRAG_TARGET" "0,0")"
    click "$TAB"
    assert_windows "a click on its tab shows $DRAG_TARGET in the stack" \
        "stacked-onto $DRAG_TARGET $DRAG_WID"
    TAB="$(remote_capture "find the tab of $DRAG_WID in the stack's bar" \
        "$REMOTE_CTL tree --json | python3 $ASSERT tab-point $DRAG_WID" "0,0")"
    click "$TAB"
    assert_windows "a click on its tab shows $DRAG_WID again" \
        "stacked-onto $DRAG_WID $DRAG_TARGET"
    action toggle-stacking
    assert_windows "toggle-stacking takes the dropped window out again" "unstacked"
    assert_windows "windows tile again after the unstack" "tiled $COUNT"

    DRAG="$(remote_capture "pick a drag onto another tile's corner" \
        "$REMOTE_CTL list-windows --json | python3 $ASSERT drag-to-corner" "0 0 0,0 0,0")"
    read -r DRAG_WID DRAG_TARGET DRAG_FROM DRAG_TO <<<"$DRAG"
    snapshot_geometry
    drag "$DRAG_FROM" "$DRAG_TO"
    assert_windows "a drop outside the middle third stacks nothing" "unstacked"
    assert_windows "the layout is unchanged after the snap-back" "geometry-unchanged $BEFORE"
    assert_windows "window $DRAG_WID snapped back into its tile (ACTUAL = FRAME)" "in-tiles"
else
    warn "send-key is not deployed or fewer than two test windows; skipping drag to stack"
fi

# --- native full screen ------------------------------------------------------------------------
# A window entering native full screen moves to a Space of its own, and SkyLight reports it
# destroyed on the way.  The WM must suspend it rather than forget it, re-flow the rest, never
# tile the transition's helper windows, and once the window leaves full screen put it and its
# neighbours back into their old tiles (the app moves its other windows while their Space is
# inactive).
if (( COUNT >= 2 )); then
    step "native full screen"
    FS_TARGET="$(remote_capture "pick a test window for full screen" \
        "$REMOTE_CTL list-windows --json | python3 $ASSERT fullscreen-candidate" $'0\tdry-run')"
    FS_PID="${FS_TARGET%%$'\t'*}"
    FS_TITLE="${FS_TARGET#*$'\t'}"
    snapshot_geometry
    run_remote "enter native full screen ($FS_TITLE)" \
        "bash $HELPER set-fullscreen $FS_PID '$FS_TITLE' true && sleep 5"
    assert_windows "the full-screen window is suspended, not listed" "count $((COUNT - 1))"
    assert_windows "the other windows still tile" "tiled $((COUNT - 1))"
    screenshot "vm-test-08-fullscreen"
    run_remote "leave native full screen ($FS_TITLE)" \
        "bash $HELPER set-fullscreen $FS_PID '$FS_TITLE' false && sleep 5"
    assert_windows "the window is managed again after full screen" "count $COUNT"
    assert_windows "every window is back in its old tile" "geometry-unchanged $BEFORE"
    assert_windows "every window sits in its tile (ACTUAL = FRAME)" "in-tiles"
else
    warn "fewer than two test windows; skipping the native full-screen phase"
fi

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
