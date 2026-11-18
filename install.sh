#!/bin/sh
# Copies the release build of doctest to ~/.local/bin.
set -e
cd "$(dirname "$0")"
[ -x target/release/doctest ] || cargo build --release
mkdir -p "$HOME/.local/bin"
cp target/release/doctest "$HOME/.local/bin/doctest"
echo "installed $HOME/.local/bin/doctest"
