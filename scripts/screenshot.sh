#!/usr/bin/env bash
# screenshot.sh — capture the VM screen over SSH and copy it back.
#
# Usage:
#   scripts/screenshot.sh                    # -> scratch/shot-<timestamp>.png
#   scripts/screenshot.sh --name spike-c     # -> scratch/spike-c.png
#   scripts/screenshot.sh --bump-approval    # print (not run) the Sequoia
#                                            # ScreenCaptureApprovals date-bump
#
# screencapture over SSH needs the Screen Recording grant for sshd
# (sshd-keygen-wrapper). If the capture is black or fails, grant it in the VM:
#   System Settings > Privacy & Security > Screen Recording > sshd-keygen-wrapper
#
# The date-bump workaround for Sequoia's 30-day re-prompt is intentionally
# confirm-gated: it edits a system plist, which the project's VM safety rules
# forbid automating.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM_HOST="${VM_HOST:-dj@192.168.64.14}"
NAME=""
BUMP=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --name)
            NAME="$2"
            shift 2
            ;;
        --bump-approval)
            BUMP=1
            shift
            ;;
        -h|--help)
            sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 64
            ;;
    esac
done

mkdir -p "$ROOT/scratch"
if [[ -n "$NAME" ]]; then
    DEST="$ROOT/scratch/$NAME.png"
else
    DEST="$ROOT/scratch/shot-$(date +%Y%m%d-%H%M%S).png"
fi

REMOTE="/tmp/cosmicmac-shot-$$.png"

echo "== capturing the VM screen"
if ! ssh -o BatchMode=yes "$VM_HOST" "screencapture -x '$REMOTE'"; then
    cat >&2 <<'EOF'
!! screencapture failed over SSH.
   Grant Screen Recording to sshd-keygen-wrapper in the VM:
     System Settings > Privacy & Security > Screen Recording
   (The process is usually named "sshd-keygen-wrapper" or "sshd".)
EOF
    exit 1
fi

scp -q "$VM_HOST:$REMOTE" "$DEST"
ssh -o BatchMode=yes "$VM_HOST" "rm -f '$REMOTE'" || true

SIZE="$(stat -f%z "$DEST" 2>/dev/null || echo 0)"
if [[ "$SIZE" -lt 10000 ]]; then
    echo "!! screenshot is only ${SIZE} bytes; it may be black (Screen Recording not granted)." >&2
    echo "   Saved anyway: $DEST" >&2
    exit 2
fi

echo "== saved $DEST (${SIZE} bytes)"

if [[ "$BUMP" -eq 1 ]]; then
    cat >&2 <<'EOF'

!! CONFIRM BEFORE RUN: Sequoia ScreenCaptureApprovals date-bump (system plist edit)
   This is NOT executed automatically. If the 30-day re-prompt becomes a problem,
   in the VM run (with sudo, after taking a snapshot):
     sudo plutil -replace \
       "$(printf '%s' 'com.apple.replayd')" \
       -date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
       /Library/Preferences/com.apple.ScreenCaptureApprovals.plist
   or use PlistBuddy on the per-app entry. Re-granting in System Settings is the
   supported path; the bump is a workaround.
EOF
fi
