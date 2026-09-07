#!/bin/sh
# Install the checkout as a normal Cargo-managed shell command.
set -eu
ROCKETRY_SOURCE_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if ! command -v cargo >/dev/null 2>&1; then
    printf '%s\n' 'Rust and Cargo are required. Install them from https://rustup.rs, then rerun this script.' >&2
    exit 1
fi
cargo install --path "$ROCKETRY_SOURCE_DIR/crates/cli" --locked "$@"
printf '\n%s\n' 'Installed Rocketry. Run: rocketry --help' 'Launch the TUI: rocketry' 'Enable workspace tools: rocketry --backend host'
printf '%s\n' 'If your shell cannot find it, add the Cargo bin directory (normally $HOME/.cargo/bin) to PATH.'
