#!/usr/bin/env bash
# deploy.sh — build on the host, sign, copy to the VM, run ax-status.
#
# The host and the VM are both aarch64-apple-darwin, so no cross-compilation is
# involved. Binaries are built for the host target, signed with the stable
# self-signed identity (so TCC grants survive), then rsynced.
#
# Usage:
#   scripts/deploy.sh                       # platform examples (ax-status, spikes)
#   scripts/deploy.sh -p cosmic-mac-wm      # extra release crates
#   scripts/deploy.sh -p cosmic-mac-wm -p cosmic-mac-session   # WM + installer
#   scripts/deploy.sh --no-build            # just sign + copy what exists
#   VM_HOST=dj@192.168.64.14 scripts/deploy.sh
#
# Every target/release/cosmic-mac-* binary that exists is deployed; Cargo's
# `.d` dep-info files next to them are not.
#
# No launchd agents are installed or restarted by this script; run
# `~/cosmicMac/bin/cosmic-mac-session install` in the VM for that.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM_HOST="${VM_HOST:-dj@192.168.64.14}"
REMOTE_DIR="${REMOTE_DIR:-cosmicMac/bin}"
BUILD=1
PACKAGES=("cosmic-mac-platform")

while [[ $# -gt 0 ]]; do
    case "$1" in
        -p|--package)
            PACKAGES+=("$2")
            shift 2
            ;;
        --no-build)
            BUILD=0
            shift
            ;;
        -h|--help)
            # The leading comment block, without the shebang.
            awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 64
            ;;
    esac
done

cd "$ROOT"

if [[ "$BUILD" -eq 1 ]]; then
    echo "== building release binaries on the host"
    args=()
    for package in "${PACKAGES[@]}"; do
        args+=(-p "$package")
    done
    cargo build --release "${args[@]}" --examples
    cargo build --release "${args[@]}" || true
fi

BINARIES=()
for candidate in \
    "$ROOT/target/release/examples/ax-status" \
    "$ROOT/target/release/examples/spike-b" \
    "$ROOT/target/release/examples/spike-c" \
    "$ROOT/target/release/examples/send-key" \
    "$ROOT/target/release/examples/socket-probe"; do
    [[ -f "$candidate" ]] && BINARIES+=("$candidate")
done
# Cargo writes `cosmic-mac-<bin>.d` dep-info files next to the binaries.
while IFS= read -r candidate; do
    [[ -n "$candidate" ]] && BINARIES+=("$candidate")
done < <(find "$ROOT/target/release" -maxdepth 1 -type f -name 'cosmic-mac-*' ! -name '*.d' 2>/dev/null || true)

if [[ ${#BINARIES[@]} -eq 0 ]]; then
    echo "!! nothing to deploy; build first" >&2
    exit 1
fi

echo "== signing ${#BINARIES[@]} binaries"
"$ROOT/scripts/sign.sh" --verify "${BINARIES[@]}"

echo "== copying to $VM_HOST:~/$REMOTE_DIR"
ssh -o BatchMode=yes "$VM_HOST" "mkdir -p \"\$HOME/$REMOTE_DIR\""
rsync -az "${BINARIES[@]}" "$VM_HOST:$REMOTE_DIR/"
ssh -o BatchMode=yes "$VM_HOST" "chmod +x \"\$HOME/$REMOTE_DIR\"/*"

echo "== remote ax-status"
set +e
STATUS_OUTPUT="$(ssh -o BatchMode=yes "$VM_HOST" "\"\$HOME/$REMOTE_DIR/ax-status\"" 2>&1)"
STATUS_CODE=$?
set -e
printf '%s\n' "$STATUS_OUTPUT"

if [[ "$STATUS_CODE" -eq 2 ]] || grep -q 'accessibility_trusted: false' <<<"$STATUS_OUTPUT"; then
    cat >&2 <<'EOF'

!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
!!  Accessibility is NOT granted for the deployed binary.
!!  Window move/resize/minimize/close and the CGEventTap will NOT work.
!!  This is expected on a fresh VM. One-time manual step in the VM GUI:
!!    System Settings > Privacy & Security > Accessibility > "+"
!!    add ~/cosmicMac/bin/ax-status (and later cosmic-mac-wm)
!!  Then re-run scripts/deploy.sh and check for accessibility_trusted: true.
!!  The grant only survives rebuilds if the identity from scripts/sign.sh is
!!  trusted and used for every build (this script does that automatically).
!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
EOF
fi

if grep -q 'displays_have_separate_spaces: true' <<<"$STATUS_OUTPUT"; then
    echo "!! 'Displays have separate Spaces' is ON; turn it off (scripts/vm-prep.sh)." >&2
fi

echo "== deploy complete: $VM_HOST:~/$REMOTE_DIR"
