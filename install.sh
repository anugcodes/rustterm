#!/usr/bin/env bash
# Install rustterm for the current user.
#
#   ./install.sh                 # build (if needed) and install to ~/.local
#   PREFIX=/usr/local ./install.sh
#
# No sudo is used; a system prefix must be writable by the current user.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
DATA_HOME="${XDG_DATA_HOME:-$PREFIX/share}"
bin="$root/target/release/rustterm"

if [[ ! -x "$bin" ]]; then
    echo "Building release binary..."
    (cd "$root" && cargo build --release --locked)
fi

install -Dm755 "$bin" "$PREFIX/bin/rustterm"
install -Dm644 "$root/assets/rustterm.desktop" \
    "$DATA_HOME/applications/rustterm.desktop"
install -Dm644 "$root/assets/rustterm.svg" \
    "$DATA_HOME/icons/hicolor/scalable/apps/rustterm.svg"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DATA_HOME/applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true
fi

echo "Installed rustterm to $PREFIX/bin/rustterm"
case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "Note: add $PREFIX/bin to PATH to run 'rustterm' from a shell." ;;
esac
