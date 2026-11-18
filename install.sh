#!/bin/sh
# Copies the release build of doctest to ~/.local/bin.
set -e
cd "$(dirname "$0")"
[ -x target/release/doctest ] || cargo build --release
mkdir -p "$HOME/.local/bin"
# copy to a new file, then rename it into place: overwriting a program that is running (an MCP server) in place
# makes macOS kill the new copy when it starts
cp target/release/doctest "$HOME/.local/bin/doctest.new"
mv -f "$HOME/.local/bin/doctest.new" "$HOME/.local/bin/doctest"
echo "installed $HOME/.local/bin/doctest ($("$HOME/.local/bin/doctest" --version))"
