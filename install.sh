#!/usr/bin/env bash
# Install or uninstall rustterm for the current user.
#
#   ./install.sh                      build and install to ~/.local
#   ./install.sh --uninstall          remove the installed files
#   ./install.sh --uninstall --purge  also remove ~/.config/rustterm
#   PREFIX=/usr/local ./install.sh
#
# No sudo is used; a system prefix must be writable by the current user.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
DATA_HOME="${XDG_DATA_HOME:-$PREFIX/share}"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
bin="$root/target/release/rustterm"

usage() {
    cat <<'EOF'
Usage: ./install.sh [--uninstall [--purge]]

  (no arguments)        build with --locked and install to $PREFIX
  --uninstall           remove the binary, launcher and icon
  --uninstall --purge   also remove $XDG_CONFIG_HOME/rustterm
  -h, --help            show this help

Set PREFIX to change the install prefix (default: ~/.local).
EOF
}

refresh_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$DATA_HOME/applications" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true
    fi
}

uninstall() {
    rm -f "$PREFIX/bin/rustterm"
    rm -f "$DATA_HOME/applications/rustterm.desktop"
    rm -f "$DATA_HOME/icons/hicolor/scalable/apps/rustterm.svg"
    refresh_caches

    echo "Removed rustterm from $PREFIX."
    if [[ "$purge" == true ]]; then
        rm -rf "${CONFIG_HOME:?}/rustterm"
        echo "Removed $CONFIG_HOME/rustterm."
    else
        echo "Kept $CONFIG_HOME/rustterm; rerun with --purge to delete settings and saved profiles."
    fi
}

uninstall_only=false
purge=false
for arg in "$@"; do
    case "$arg" in
        --uninstall|uninstall) uninstall_only=true ;;
        --purge) purge=true ;;
        -h|--help) usage; exit 0 ;;
        *)
            echo "Unknown argument: $arg" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ "$purge" == true && "$uninstall_only" == false ]]; then
    echo "--purge requires --uninstall" >&2
    exit 2
fi

if [[ "$uninstall_only" == true ]]; then
    uninstall
    exit 0
fi

echo "Building release binary..."
(cd "$root" && cargo build --release --locked)

install -Dm755 "$bin" "$PREFIX/bin/rustterm"
install -Dm644 "$root/assets/rustterm.desktop" \
    "$DATA_HOME/applications/rustterm.desktop"
install -Dm644 "$root/assets/rustterm.svg" \
    "$DATA_HOME/icons/hicolor/scalable/apps/rustterm.svg"
refresh_caches

echo "Installed rustterm to $PREFIX/bin/rustterm"
case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "Note: add $PREFIX/bin to PATH to run 'rustterm' from a shell." ;;
esac
