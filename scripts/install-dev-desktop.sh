#!/bin/sh
# Registers a .desktop entry for development builds so GNOME/KDE show the
# Carteiro icon in the dock (Wayland matches windows to .desktop files by
# app ID, which for a Tauri app is the program name unless GTK app IDs are
# enabled; enabling them breaks tray menus). Installed packages ship their own
# entry; this is only for `cargo tauri dev`.
# Undo with: rm ~/.local/share/applications/carteiro.desktop
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
APP_DIR="$HOME/.local/share/applications"
mkdir -p "$ICON_DIR" "$APP_DIR"
cp "$ROOT/app-icon.svg" "$ICON_DIR/carteiro.svg"
# Clean up entries from before the rename.
rm -f "$APP_DIR/vantage.desktop" "$ICON_DIR/vantage.svg" "$APP_DIR/dev.thunderbonx.mail.desktop" "$ICON_DIR/dev.thunderbonx.mail.svg" "$APP_DIR/dev.carteiro.mail.desktop" "$ICON_DIR/dev.carteiro.mail.svg"
cat > "$APP_DIR/carteiro.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Carteiro (dev)
Comment=Email and calendar
Exec=$ROOT/target/debug/carteiro
Icon=carteiro
StartupWMClass=carteiro
Categories=Network;Email;
Terminal=false
DESKTOP
update-desktop-database "$APP_DIR" 2>/dev/null || true
gtk-update-icon-cache -q "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
echo "Installed. Restart 'cargo tauri dev' to pick up the icon."
