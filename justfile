# cosmicMac workspace tasks.
#
# Only platform-crate tasks live here for now; the WM/layout tasks will be added
# by their owners. Host commands never touch the VM; VM commands are explicit.

set shell := ["bash", "-uc"]

default: build

# Build the platform crate and its spike binaries.
build:
    cargo build -p cosmic-mac-platform --examples

# Release build of the deployable examples.
release:
    cargo build --release -p cosmic-mac-platform --examples

# Lint only this crate (the workspace may be mid-write elsewhere).
lint:
    cargo clippy -p cosmic-mac-platform -- -D warnings

# Format only this crate.
fmt:
    cargo fmt -p cosmic-mac-platform

fmt-check:
    cargo fmt -p cosmic-mac-platform -- --check

# Host-only unit tests (geometry and coordinate conversions).
test:
    cargo test -p cosmic-mac-platform

# Build, sign, copy to the VM, run ax-status.
deploy:
    ./scripts/deploy.sh

# Capture the VM screen into scratch/.
screenshot:
    ./scripts/screenshot.sh

screenshot-name name:
    ./scripts/screenshot.sh --name {{name}}

# Print the VM preparation steps (dry run; add --apply manually).
vm-prep:
    ./scripts/vm-prep.sh

# Create/verify the self-signed code-signing identity and sign current binaries.
sign:
    ./scripts/sign.sh --verify

# Smoke test the deployed spikes in the VM.
vm-test:
    ./scripts/vm-test.sh

# Run spike-b locally on the host (read-only subcommands only).
spike-b args="selftest":
    cargo run -q -p cosmic-mac-platform --example spike-b -- {{args}}

# Print the deployed binary status without rebuilding.
ax-status:
    ./scripts/deploy.sh --no-build
