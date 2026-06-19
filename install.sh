#!/bin/sh
# install.sh — build and install the `chezzi` toolchain into ~/.cargo/bin.
#
# Usage:  ./install.sh
#
# Requires cargo (the Rust toolchain). Installs via `cargo install --path .`, which places the
# `chezzi` binary in ~/.cargo/bin — already on PATH for anyone who installed Rust via rustup.

set -e

# Run from the repo root (this script's own directory) so `--path .` is correct from any cwd.
cd "$(dirname "$0")"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: 'cargo' was not found on your PATH." >&2
    echo "       Chezzi is built with Rust. Install the Rust toolchain first:" >&2
    echo "         https://rustup.rs" >&2
    echo "       (rustup also puts ~/.cargo/bin on your PATH.)" >&2
    exit 1
fi

echo "Installing chezzi via 'cargo install --path .' ..."
cargo install --path .

echo ""
echo "Done. The 'chezzi' binary is installed in ~/.cargo/bin."
echo "Ensure ~/.cargo/bin is on your PATH (rustup adds it for you; otherwise add it to your shell profile)."
echo "Try:  chezzi help"
