#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

# The installed daemon needs real capture and VA-API, not the test-pattern fallback.
cargo build --release --locked -p linux-quest-host -p deskquest-desktop \
  --features linux-quest-host/mdns,linux-quest-host/pipewire-capture,linux-quest-host/vaapi
cargo deb -p linux-quest-host --no-build
cargo deb -p deskquest-desktop --no-build
cargo generate-rpm -p crates/host

# A successful build must contain the actual GUI, launcher and icon.
desktop_package=$(find target/debian -maxdepth 1 -name 'deskquest_*.deb' -print -quit)
test -n "$desktop_package"
contents=$(dpkg-deb --contents "$desktop_package")
for path in usr/bin/deskquest usr/share/applications/org.deskquest.Desktop.desktop usr/share/icons/hicolor/512x512/apps/org.deskquest.Desktop.png; do
  if ! grep -Fq "$path" <<< "$contents"; then
    echo "Missing desktop package payload: $path" >&2
    exit 1
  fi
done
