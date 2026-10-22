#!/bin/sh
# Build gsvn in release mode and install it for the current user:
#   ~/.local/bin/gsvn
#   ~/.local/share/applications/io.github.gsvn.Gsvn.desktop
#   ~/.local/share/icons/hicolor/{scalable,symbolic}/apps/io.github.gsvn.Gsvn*.svg
#
# Usage: scripts/install-local.sh [--uninstall]
set -eu

APP_ID=io.github.gsvn.Gsvn
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN_DIR=${XDG_BIN_HOME:-$HOME/.local/bin}
DATA_DIR=${XDG_DATA_HOME:-$HOME/.local/share}
ICON_DIR=$DATA_DIR/icons/hicolor

refresh_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$DATA_DIR/applications" || true
    fi
    if command -v gtk4-update-icon-cache >/dev/null 2>&1; then
        gtk4-update-icon-cache -q -t -f "$ICON_DIR" || true
    elif command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f "$ICON_DIR" || true
    fi
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN_DIR/gsvn" \
        "$DATA_DIR/applications/$APP_ID.desktop" \
        "$ICON_DIR/scalable/apps/$APP_ID.svg" \
        "$ICON_DIR/symbolic/apps/$APP_ID-symbolic.svg"
    refresh_caches
    echo "gsvn uninstalled."
    exit 0
fi

cargo build --release --manifest-path "$ROOT/Cargo.toml" -p gsvn

install -Dm755 "$ROOT/target/release/gsvn" "$BIN_DIR/gsvn"
# Absolute Exec path so the launcher works even if ~/.local/bin is not in
# the session PATH.
install -d "$DATA_DIR/applications"
sed "s|^Exec=gsvn|Exec=$BIN_DIR/gsvn|" "$ROOT/data/$APP_ID.desktop" \
    > "$DATA_DIR/applications/$APP_ID.desktop"
chmod 644 "$DATA_DIR/applications/$APP_ID.desktop"
install -Dm644 "$ROOT/data/icons/hicolor/scalable/apps/$APP_ID.svg" \
    "$ICON_DIR/scalable/apps/$APP_ID.svg"
install -Dm644 "$ROOT/data/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg" \
    "$ICON_DIR/symbolic/apps/$APP_ID-symbolic.svg"
refresh_caches

echo "Installed gsvn to $BIN_DIR/gsvn"
