#!/usr/bin/env bash
# sign.sh — sign cosmicMac binaries with a stable self-signed identity.
#
# Why: TCC (Accessibility, Screen Recording) keys the grant on the code
# signature's designated requirement. Ad-hoc signatures change on every build,
# so the grant would be lost on every deploy. A self-signed certificate with a
# stable CN gives every build the same designated requirement:
#
#   identifier "com.cosmicmac.<binary>" and certificate root = H"<sha1>"
#
# A dedicated keychain is used so signing never prompts for the login keychain:
#   ~/Library/Application Support/cosmicMac/signing/cosmicMac-signing.keychain-db
# Its random password is stored next to it (mode 600). The keychain is added to
# the user search list and its key partition list is set for codesign.
#
# Usage:
#   scripts/sign.sh [--create-identity] [--trust] [--verify] [binary ...]
#
# Default binaries: target/release/examples/{ax-status,spike-b,spike-c} and
# target/release/cosmic-mac-* when present.
#
# --create-identity creates the key/cert and imports it into the dedicated
#   keychain (no sudo, no GUI). Safe to re-run.
# --trust adds the certificate to the user trust settings. This needs one
#   GUI/keychain authorization; it is never run unless --trust is passed.
#
# Without --trust, codesign still produces a valid signature and a stable
# designated requirement (`codesign --verify` passes). Trust matters for
# Gatekeeper and for some TCC versions; if Accessibility is forgotten after a
# redeploy, run `scripts/sign.sh --trust` once in the VM/host GUI session.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SIGN_DIR="$HOME/Library/Application Support/cosmicMac/signing"
KEYCHAIN="$SIGN_DIR/cosmicMac-signing.keychain-db"
PASSWORD_FILE="$SIGN_DIR/keychain-password"
IDENTITY_NAME="${IDENTITY_NAME:-cosmicMac Dev}"
CREATE=0
TRUST=0
VERIFY=0
BINARIES=()

for arg in "$@"; do
    case "$arg" in
        --create-identity) CREATE=1 ;;
        --trust) TRUST=1 ;;
        --verify) VERIFY=1 ;;
        -h|--help)
            sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        -*)
            echo "unknown option: $arg" >&2
            exit 64
            ;;
        *) BINARIES+=("$arg") ;;
    esac
done

mkdir -p "$SIGN_DIR"
chmod 700 "$SIGN_DIR"

keychain_password() {
    if [[ ! -f "$PASSWORD_FILE" ]]; then
        openssl rand -hex 16 >"$PASSWORD_FILE"
        chmod 600 "$PASSWORD_FILE"
    fi
    cat "$PASSWORD_FILE"
}

create_identity() {
    echo "== creating self-signed code-signing identity '$IDENTITY_NAME'"
    cat >"$SIGN_DIR/openssl.cnf" <<'EOF'
[req]
distinguished_name = dn
x509_extensions = v3_req
prompt = no
[dn]
CN = cosmicMac Dev
O = cosmicMac
[v3_req]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
EOF
    if [[ ! -f "$SIGN_DIR/key.pem" || ! -f "$SIGN_DIR/cert.pem" ]]; then
        openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
            -keyout "$SIGN_DIR/key.pem" \
            -out "$SIGN_DIR/cert.pem" \
            -config "$SIGN_DIR/openssl.cnf"
        chmod 600 "$SIGN_DIR/key.pem"
    else
        echo "   key/cert already exist in $SIGN_DIR"
    fi

    # macOS Security.framework only understands legacy PKCS#12 encryption.
    # OpenSSL 3+ needs -legacy; LibreSSL/older OpenSSL do not have the flag.
    local legacy=()
    if openssl pkcs12 -help 2>&1 | grep -q -- '-legacy'; then
        legacy=(-legacy)
    fi
    openssl pkcs12 -export "${legacy[@]}" \
        -inkey "$SIGN_DIR/key.pem" \
        -in "$SIGN_DIR/cert.pem" \
        -name "$IDENTITY_NAME" \
        -out "$SIGN_DIR/identity.p12" \
        -passout pass:cosmicmac

    local password
    password="$(keychain_password)"
    if [[ ! -f "$KEYCHAIN" ]]; then
        security create-keychain -p "$password" "$KEYCHAIN"
        security set-keychain-settings -lut 21600 "$KEYCHAIN"
    fi
    security unlock-keychain -p "$password" "$KEYCHAIN"
    security import "$SIGN_DIR/identity.p12" \
        -k "$KEYCHAIN" \
        -P cosmicmac \
        -T /usr/bin/codesign \
        -T /usr/bin/security
    security set-key-partition-list \
        -S apple-tool:,apple: -s \
        -k "$password" "$KEYCHAIN" >/dev/null
    # Make the identity discoverable without --keychain as well.
    local existing
    existing="$(security list-keychains -d user | sed 's/^[[:space:]]*//;s/"//g')"
    if ! grep -qx "$KEYCHAIN" <<<"$existing"; then
        # shellcheck disable=SC2086
        security list-keychains -d user -s "$KEYCHAIN" $existing
    fi
    echo "   identity imported into $KEYCHAIN"
}

trust_identity() {
    echo
    echo "!! CONFIRM BEFORE RUN: trusting the self-signed certificate"
    echo "   command: security add-trusted-cert -r trustRoot -k \"$HOME/Library/Keychains/login.keychain-db\" \"$SIGN_DIR/cert.pem\""
    echo "   This is a user trust change and may show a GUI/keychain prompt."
    if [[ "$TRUST" -eq 1 ]]; then
        security add-trusted-cert -r trustRoot \
            -k "$HOME/Library/Keychains/login.keychain-db" \
            "$SIGN_DIR/cert.pem" || {
            echo "   !! trust failed (headless session?); run the command in a GUI Terminal." >&2
        }
    else
        echo "   (pass --trust to attempt it; otherwise it stays a manual step)"
    fi
}

find_identity() {
    # Deliberately not -v: an untrusted self-signed identity is still usable for
    # signing and produces the stable designated requirement TCC needs.
    security find-identity -p codesigning "$KEYCHAIN" 2>/dev/null \
        | sed -n "s/.*\"\\($IDENTITY_NAME[^\"]*\\)\".*/\\1/p" \
        | head -1
}

if [[ "$CREATE" -eq 1 ]]; then
    create_identity
fi

if [[ ! -f "$KEYCHAIN" ]]; then
    echo "!! no signing keychain at $KEYCHAIN" >&2
    echo "   Run: scripts/sign.sh --create-identity" >&2
    exit 1
fi

PASSWORD="$(keychain_password)"
security unlock-keychain -p "$PASSWORD" "$KEYCHAIN"

IDENTITY="$(find_identity || true)"
if [[ -z "$IDENTITY" ]]; then
    echo "!! no '$IDENTITY_NAME' identity in $KEYCHAIN" >&2
    echo "   Run: scripts/sign.sh --create-identity" >&2
    exit 1
fi
echo "== signing identity: $IDENTITY"

if [[ "$TRUST" -eq 1 ]]; then
    trust_identity
fi

if [[ ${#BINARIES[@]} -eq 0 ]]; then
    for candidate in \
        "$ROOT/target/release/examples/ax-status" \
        "$ROOT/target/release/examples/spike-b" \
        "$ROOT/target/release/examples/spike-c"; do
        [[ -f "$candidate" ]] && BINARIES+=("$candidate")
    done
    while IFS= read -r candidate; do
        [[ -n "$candidate" ]] && BINARIES+=("$candidate")
    done < <(find "$ROOT/target/release" -maxdepth 1 -type f -name 'cosmic-mac-*' 2>/dev/null || true)
fi

if [[ ${#BINARIES[@]} -eq 0 ]]; then
    echo "!! no binaries to sign (build first: cargo build --release -p cosmic-mac-platform --examples)" >&2
    exit 1
fi

for binary in "${BINARIES[@]}"; do
    if [[ ! -f "$binary" ]]; then
        echo "!! missing $binary" >&2
        exit 1
    fi
    name="$(basename "$binary")"
    echo "== signing $name"
    codesign --force \
        --keychain "$KEYCHAIN" \
        --sign "$IDENTITY" \
        --identifier "com.cosmicmac.$name" \
        --timestamp=none \
        "$binary"
    if [[ "$VERIFY" -eq 1 ]]; then
        codesign --verify --verbose=2 "$binary" || true
        echo "   designated requirement:"
        codesign -d -r- "$binary" 2>&1 | sed 's/^/     /' || true
    fi
done

echo
echo "== done. Keep the same identity across rebuilds so the TCC grant survives."
echo "   Acceptance test: grant Accessibility once, rebuild, redeploy, run ax-status;"
echo "   accessibility_trusted must still be true."
