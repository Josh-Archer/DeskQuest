#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

shortcut=false
build=true
tray=false
for arg in "$@"; do
  case "$arg" in
    --desktop-shortcut) shortcut=true ;;
    --no-build) build=false ;;
    --tray) tray=true ;;
    *) echo "Usage: $0 [--desktop-shortcut] [--tray] [--no-build]" >&2; exit 2 ;;
  esac
done
if "$build"; then
  cargo build --release --locked -p linux-quest-desktop
  if "$tray"; then
    cargo build --release --locked -p linux-quest-host
  fi
fi
binary="${CARGO_TARGET_DIR:-target}/release/linux-quest"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
bin_dir="$HOME/.local/bin"
install -Dm755 "$binary" "$bin_dir/linux-quest"
desktop-file-install --dir="$data_dir/applications" \
  --set-key=Exec --set-value="$bin_dir/linux-quest" \
  packaging/desktop/org.linuxquest.Desktop.desktop
install -Dm644 packaging/icons/hicolor/512x512/apps/org.linuxquest.Desktop.png \
  "$data_dir/icons/hicolor/512x512/apps/org.linuxquest.Desktop.png"
gtk-update-icon-cache --force --ignore-theme-index "$data_dir/icons/hicolor"
update-desktop-database "$data_dir/applications"
if "$tray"; then
  # This private binary runs only the tray; it does not replace an existing daemon.
  lib_dir="$HOME/.local/lib/linux-quest"
  install -Dm755 "${CARGO_TARGET_DIR:-target}/release/linux-quest-host" \
    "$lib_dir/linux-quest-host"
  desktop-file-install --dir="${XDG_CONFIG_HOME:-$HOME/.config}/autostart" \
    --set-key=Exec --set-value="\"$lib_dir/linux-quest-host\" tray" \
    packaging/desktop/org.linuxquest.Tray.desktop
fi
if "$shortcut"; then
  desktop_dir=$(xdg-user-dir DESKTOP)
  if [ "$desktop_dir" = "$HOME" ] || [ -z "$desktop_dir" ]; then
    echo "No desktop folder is configured; launcher installed in the app menu." >&2
  else
    install -Dm755 "$data_dir/applications/org.linuxquest.Desktop.desktop" \
      "$desktop_dir/Linux Quest.desktop"
    gio set "$desktop_dir/Linux Quest.desktop" metadata::trusted true || \
      echo "Choose Allow Launching on the desktop shortcut if your desktop asks." >&2
  fi
fi
