#!/usr/bin/env bash
# bundle.sh — assemble and sign cosmic-mac-launcher.app.
#
# The launcher must run from a real app bundle: only then does macOS see its
# bundle identifier and LSUIElement, so it starts as an agent app (no Dock
# icon) instead of winit forcing the Regular activation policy at launch.
#
# Layout (from target/release/cosmic-mac-launcher and
# crates/cosmic-mac-launcher/Info.plist):
#
#   <out>/cosmic-mac-launcher.app/Contents/Info.plist
#   <out>/cosmic-mac-launcher.app/Contents/PkgInfo
#   <out>/cosmic-mac-launcher.app/Contents/MacOS/cosmic-mac-launcher
#
# The bundle is rebuilt from scratch, checked with `plutil -lint` and signed
# with scripts/sign.sh (identifier com.cosmicmac.cosmic-mac-launcher, the
# bundle identifier). scripts/deploy.sh runs this and rsyncs the bundle.
#
# Usage:
#   scripts/bundle.sh [--no-sign] [--binary PATH] [--out DIR]
#
# Defaults: --binary target/release/cosmic-mac-launcher,
#           --out target/release/bundles.
# Prints the bundle path on the last line.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$ROOT/target/release/cosmic-mac-launcher"
OUT="$ROOT/target/release/bundles"
INFO_PLIST="$ROOT/crates/cosmic-mac-launcher/Info.plist"
SIGN=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-sign)
            SIGN=0
            shift
            ;;
        --binary)
            BINARY="$2"
            shift 2
            ;;
        --out)
            OUT="$2"
            shift 2
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

if [[ ! -f "$BINARY" ]]; then
    echo "!! missing $BINARY (build first: cargo build --release -p cosmic-mac-launcher)" >&2
    exit 1
fi

NAME="$(basename "$BINARY")"
BUNDLE="$OUT/$NAME.app"
echo "== assembling $BUNDLE"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS"
cp "$INFO_PLIST" "$BUNDLE/Contents/Info.plist"
printf 'APPL????' >"$BUNDLE/Contents/PkgInfo"
cp "$BINARY" "$BUNDLE/Contents/MacOS/$NAME"
chmod 755 "$BUNDLE/Contents/MacOS/$NAME"

plutil -lint "$BUNDLE/Contents/Info.plist"
EXECUTABLE="$(plutil -extract CFBundleExecutable raw "$BUNDLE/Contents/Info.plist")"
if [[ "$EXECUTABLE" != "$NAME" ]]; then
    echo "!! Info.plist names the executable '$EXECUTABLE', expected '$NAME'" >&2
    exit 1
fi

if [[ "$SIGN" -eq 1 ]]; then
    "$ROOT/scripts/sign.sh" --verify "$BUNDLE"
fi

echo "$BUNDLE"
