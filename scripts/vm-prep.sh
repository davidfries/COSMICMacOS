#!/usr/bin/env bash
# vm-prep.sh — prepare the macOS test VM for cosmicMac.
#
# Idempotent: every step prints what it wants to do and checks the current
# state first. Nothing that changes the system is executed unless --apply is
# passed, and every sudo / GUI step is printed with a loud CONFIRM BEFORE RUN
# marker first. The two GUI grants (Accessibility, Screen Recording) are never
# automated: they are one-time clicks in System Settings.
#
# Usage:
#   scripts/vm-prep.sh                 # dry run: print every step
#   scripts/vm-prep.sh --apply         # apply the non-GUI steps
#   scripts/vm-prep.sh --apply --open-panes
#   VM_HOST=dj@192.168.64.14 scripts/vm-prep.sh
#
# --open-panes opens the Accessibility and Screen Recording panes in the VM's
# GUI session; it does not grant anything.

set -euo pipefail

VM_HOST="${VM_HOST:-dj@192.168.64.14}"
APPLY=0
OPEN_PANES=0

for arg in "$@"; do
    case "$arg" in
        --apply) APPLY=1 ;;
        --open-panes) OPEN_PANES=1 ;;
        -h|--help)
            sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 64
            ;;
    esac
done

SSH=(ssh -o BatchMode=yes -o ConnectTimeout=8 "$VM_HOST")

step() { printf '\n== %s\n' "$*"; }
note() { printf '   %s\n' "$*"; }
confirm() { printf '\n!! CONFIRM BEFORE RUN: %s\n' "$*"; }
remote() { "${SSH[@]}" "$@"; }

# Run a command on the VM only with --apply; otherwise print it.
gated() {
    local description="$1"
    shift
    if [[ "$APPLY" -eq 1 ]]; then
        printf '>> %s\n' "$description"
        remote "$@"
    else
        confirm "$description"
        printf '   command: ssh %s %q\n' "$VM_HOST" "$*"
    fi
}

# Same, but for sudo: refuse even with --apply unless passwordless sudo works.
gated_sudo() {
    local description="$1"
    shift
    if [[ "$APPLY" -eq 1 ]]; then
        if remote 'sudo -n true' >/dev/null 2>&1; then
            printf '>> %s\n' "$description"
            remote "sudo $*"
        else
            confirm "$description (sudo needs a password; run it manually in the VM)"
            printf '   command: ssh -t %s "sudo %s"\n' "$VM_HOST" "$*"
        fi
    else
        confirm "$description"
        printf '   command: ssh -t %s "sudo %s"\n' "$VM_HOST" "$*"
    fi
}

step "check SSH connectivity to $VM_HOST"
remote true
note "ok"

step "collect VM facts (read-only)"
remote 'sw_vers; echo "---"; csrutil status; echo "---"; sysctl -n hw.ncpu hw.memsize; echo "---"; df -h / | tail -1; echo "---"; who'

step "recommended UTM settings (manual, not changed by this script)"
note "CPUs: 4 (currently shown above), RAM: 8 GB, disk: 64 GB."
note "Change these in UTM > Settings while the VM is shut down."
note "Snapshot: stop the VM, then on the host:"
note "  cp -c <bundle>.utm <bundle>-snapshot.utm   # APFS clone"

step "auto-login (sudo)"
CURRENT_AUTOLOGIN="$(remote 'defaults read /Library/Preferences/com.apple.loginwindow autoLoginUser 2>/dev/null || true')"
if [[ "$CURRENT_AUTOLOGIN" == "dj" ]]; then
    note "already enabled for dj"
else
    gated_sudo "enable auto-login for dj (requires the account password at next boot)" \
        "defaults write /Library/Preferences/com.apple.loginwindow autoLoginUser -string dj"
fi

step "disable display sleep (sudo)"
CURRENT_DISPLAYSLEEP="$(remote 'pmset -g | awk "/displaysleep/ {print \$2}"' || true)"
if [[ "$CURRENT_DISPLAYSLEEP" == "0" ]]; then
    note "already 0"
else
    gated_sudo "set displaysleep to 0 (keeps the GUI session alive for SSH tests)" \
        "pmset displaysleep 0"
fi

step "disable screen saver (user defaults)"
CURRENT_SAVER="$(remote 'defaults -currentHost read com.apple.screensaver idleTime 2>/dev/null || true')"
if [[ "$CURRENT_SAVER" == "0" ]]; then
    note "already 0"
else
    gated "set screensaver idleTime to 0" \
        "defaults -currentHost write com.apple.screensaver idleTime -int 0"
fi

step "turn off 'Displays have separate Spaces' (requires logout)"
SEPARATE="$(remote 'defaults read com.apple.spaces spans-displays 2>/dev/null || echo missing')"
if [[ "$SEPARATE" == "1" ]]; then
    note "already off (spans-displays = 1)"
else
    gated "write com.apple.spaces spans-displays = true (then log out/in once)" \
        "defaults write com.apple.spaces spans-displays -bool true"
fi

step "disable macOS native tiling defaults (user defaults)"
for key in EnableTilingByEdgeDrag EnableTopTilingByEdgeDrag EnableTilingOptionAccelerator; do
    CURRENT="$(remote "defaults read com.apple.WindowManager $key 2>/dev/null || echo missing")"
    if [[ "$CURRENT" == "0" ]]; then
        note "$key already false"
    else
        gated "disable com.apple.WindowManager.$key" \
            "defaults write com.apple.WindowManager $key -bool false"
    fi
done

step "create the deploy directory"
if remote 'test -d "$HOME/cosmicMac/bin"'; then
    note "~/cosmicMac/bin already exists"
else
    if [[ "$APPLY" -eq 1 ]]; then
        remote 'mkdir -p "$HOME/cosmicMac/bin"'
        note "created ~/cosmicMac/bin"
    else
        note "would create ~/cosmicMac/bin"
    fi
fi

step "one-time GUI grants (never automated)"
cat <<'EOF'
   Accessibility (for the WM daemon and spike binaries):
     1. Deploy once: scripts/deploy.sh
     2. In the VM: System Settings > Privacy & Security > Accessibility
     3. Click "+", add ~/cosmicMac/bin/cosmic-mac-wm (or spike-b while spiking).
     4. Re-run: ~/cosmicMac/bin/ax-status   -> accessibility_trusted: true
   Screen Recording (for screencapture over SSH):
     1. In the VM: System Settings > Privacy & Security > Screen Recording
     2. Enable "sshd-keygen-wrapper" (or Terminal, depending on how you run it).
     3. Re-run scripts/screenshot.sh; Sequoia re-prompts every ~30 days.
   Both grants are keyed on the code signature. Keep the same signing identity
   (scripts/sign.sh) or you must grant Accessibility again after every rebuild.
EOF

if [[ "$OPEN_PANES" -eq 1 ]]; then
    step "opening the privacy panes in the VM GUI session"
    remote 'open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"' || true
    remote 'open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"' || true
    note "click the + button and add the deployed binary"
else
    note "(pass --open-panes to open both privacy panes over SSH)"
fi

step "done"
if [[ "$APPLY" -eq 0 ]]; then
    note "dry run: nothing was changed. Re-run with --apply for the non-GUI steps."
fi
