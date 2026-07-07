#!/bin/sh
# Run cargo inside docker. The host has no C toolchain, so linking happens in
# the rust image; the registry cache persists in a named volume and target/
# lives on the bind mount, so builds are incremental.
#
# Usage: scripts/cargo.sh <any cargo args>, e.g. scripts/cargo.sh check
set -eu
cd "$(dirname "$0")/.."
exec docker run --rm \
    -v "$(pwd)":/w \
    -v sluice-cargo-registry:/usr/local/cargo/registry \
    -w /w \
    rust:1.96-bookworm \
    cargo "$@"
