#!/bin/sh
# Run Cargo, rustfmt, and Clippy inside Docker. The registry cache persists
# in a named volume and target/ stays on the bind mount for incremental builds.
#
# Usage: scripts/cargo.sh <any cargo args>, e.g. scripts/cargo.sh check
set -eu
cd "$(dirname "$0")/.."
docker build -q -t sluice-rust-dev -f scripts/Dockerfile.cargo . > /dev/null
exec docker run --rm \
    -v "$(pwd)":/w \
    -v sluice-cargo-registry:/usr/local/cargo/registry \
    -w /w \
    sluice-rust-dev \
    cargo "$@"
