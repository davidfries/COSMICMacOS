#!/usr/bin/env bash
# vm-test.sh — drive the deployed spike binaries in the VM.
#
# This is the milestone-0/1 smoke test. It never uses sudo, never clicks the
# GUI, and only opens TextEdit windows when --windows is passed (so a normal run
# cannot disturb whatever is already on the VM desktop).
#
# Usage:
#   scripts/vm-test.sh                     # status + list + selftest + screenshot
#   scripts/vm-test.sh --windows 2         # also open 2 TextEdit windows
#   scripts/vm-test.sh --move-test         # move/park a TextEdit window (needs AX)
#   scripts/vm-test.sh --no-screenshot
#
# Requires: scripts/deploy.sh has run; Screen Recording granted for screenshots;
# Accessibility granted for the move/park steps.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM_HOST="${VM_HOST:-dj@192.168.64.14}"
REMOTE_BIN="${REMOTE_BIN:-cosmicMac/bin}"
WINDOWS=0
MOVE_TEST=0
SCREENSHOT=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --windows) WINDOWS="$2"; shift 2 ;;
        --move-test) MOVE_TEST=1; shift ;;
        --no-screenshot) SCREENSHOT=0; shift ;;
        -h|--help)
            sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 64 ;;
    esac
done

SSH=(ssh -o BatchMode=yes "$VM_HOST")
remote() { "${SSH[@]}" "$@"; }
step() { printf '\n== %s\n' "$*"; }

step "check deployed binaries"
remote "test -x \"\$HOME/$REMOTE_BIN/ax-status\"" || {
    echo "!! ax-status not deployed; run scripts/deploy.sh first" >&2
    exit 1
}

step "ax-status"
set +e
AX_OUT="$(remote "\"\$HOME/$REMOTE_BIN/ax-status\"" 2>&1)"
AX_CODE=$?
set -e
printf '%s\n' "$AX_OUT"
AX_TRUSTED=0
grep -q 'accessibility_trusted: true' <<<"$AX_OUT" && AX_TRUSTED=1

step "spike-b selftest"
remote "\"\$HOME/$REMOTE_BIN/spike-b\" selftest" || true

if [[ "$WINDOWS" -gt 0 ]]; then
    step "opening $WINDOWS TextEdit windows"
    for index in $(seq 1 "$WINDOWS"); do
        remote "printf 'cosmicMac test window $index\\n' > /tmp/cosmicmac-test-$index.txt && open -n -a TextEdit /tmp/cosmicmac-test-$index.txt"
        sleep 1
    done
    step "window list"
    remote "\"\$HOME/$REMOTE_BIN/spike-b\" list" || true
fi

if [[ "$MOVE_TEST" -eq 1 ]]; then
    step "move/park test (needs Accessibility)"
    if [[ "$AX_TRUSTED" -eq 1 ]]; then
        WID="$(remote "\"\$HOME/$REMOTE_BIN/spike-b\" list" \
            | awk '/TextEdit/ && /wid=/ {gsub("wid=", "", $1); print $1; exit}')"
        if [[ -n "${WID:-}" ]]; then
            echo "using TextEdit wid=$WID"
            remote "\"\$HOME/$REMOTE_BIN/spike-b\" move $WID 120 160 760 520" || true
            remote "\"\$HOME/$REMOTE_BIN/spike-b\" park $WID --seconds 4" || true
        else
            echo "no TextEdit window found; pass --windows 1" >&2
        fi
    else
        echo "!! skipped: Accessibility is not granted (expected before the GUI step)" >&2
    fi
fi

step "spike-c overlay (own raw SkyLight target, 6s)"
remote "nohup \"\$HOME/$REMOTE_BIN/spike-c\" --seconds 6 >/tmp/cosmicmac-spike-c.log 2>&1 &" || true
sleep 2
if [[ "$SCREENSHOT" -eq 1 ]]; then
    step "screenshot"
    "$ROOT/scripts/screenshot.sh" --name vm-test-spike-c || true
fi
sleep 1
remote "cat /tmp/cosmicmac-spike-c.log 2>/dev/null | tail -20" || true

step "summary"
if [[ "$AX_TRUSTED" -eq 1 ]]; then
    echo "Accessibility: granted (window control + event tap available)"
else
    echo "Accessibility: NOT granted (move/park/tap are gated; SLS/overlay paths still work)"
fi
echo "Logs: ssh $VM_HOST 'cat /tmp/cosmicmac-spike-c.log'"
