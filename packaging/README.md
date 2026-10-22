# Packaging and local builds

Files for running `linux-quest-host` as a background service on Ubuntu / systemd desktops.

| Path | Purpose |
| :--- | :--- |
| `systemd/linux-quest-host.service` | systemd **user** unit running `linux-quest-host daemon` |
| `systemd/linux-quest-tray.service` | systemd **user** unit running `linux-quest-host tray` |
| `desktop/org.linuxquest.Desktop.desktop` | Desktop entry for the `linux-quest` app |
| `icons/hicolor/512x512/apps/org.linuxquest.Desktop.png` | Linux Quest app icon for launchers and docks |
| `udev/99-uinput.rules` | Grants the `input` group access to `/dev/uinput` |
| `modules-load/linux-quest.conf` | Loads the `uinput` module at boot |
| `debian/postinst`, `debian/copyright` | Debian maintainer script (reloads udev) and copyright file, used by `cargo deb` |
| `arch/PKGBUILD` | AUR package recipe |
| `flatpak/` | Flatpak manifest and vendored Cargo source manifest |

The tray icon (`linux-quest-host tray`) is part of every build. See [Tray icon](#tray-icon) below.

## Ubuntu desktop app and logo

The GTK desktop app embeds the logo so its window and header show it even when run
directly with `cargo run -p linux-quest-desktop`. To install the app, launcher and
icon for your user:

```bash
sudo apt-get install libgtk-4-dev libadwaita-1-dev libglib2.0-dev-bin
cargo install --locked --path crates/desktop --bin linux-quest --root ~/.local
install -Dm644 packaging/desktop/org.linuxquest.Desktop.desktop \
  ~/.local/share/applications/org.linuxquest.Desktop.desktop
install -Dm644 packaging/icons/hicolor/512x512/apps/org.linuxquest.Desktop.png \
  ~/.local/share/icons/hicolor/512x512/apps/org.linuxquest.Desktop.png
gtk-update-icon-cache --force --ignore-theme-index ~/.local/share/icons/hicolor
```

Put `~/.local/bin` on your `PATH` if it is not already there, then open **Linux
Quest** from the application launcher. The daemon packages below do not include
the desktop app.

The full-resolution transparent logo is in
[`assets/branding/linux-quest-logo.png`](../assets/branding/linux-quest-logo.png).
To regenerate the 512-pixel desktop icon without changing the master, run:

```bash
ffmpeg -v error -y -i assets/branding/linux-quest-logo.png \
  -vf scale=512:512:flags=lanczos -frames:v 1 -update 1 \
  packaging/icons/hicolor/512x512/apps/org.linuxquest.Desktop.png
```

## 1. Install the binary

```bash
cargo install --path crates/host --bin linux-quest-host --root ~/.local
sudo install -m 0755 ~/.local/bin/linux-quest-host /usr/bin/linux-quest-host
```

The unit's `ExecStart` expects `/usr/bin/linux-quest-host`. If you install elsewhere, override it:

```bash
systemctl --user edit linux-quest-host
# then add, in the drop-in:
# [Service]
# ExecStart=
# ExecStart=%h/.cargo/bin/linux-quest-host daemon
```

## 2. Configure `/dev/uinput`

Follow the udev steps in the main [README](../README.md#-kernel-input-injection-devuinput-setup), or install the rule shipped here:

```bash
sudo install -m 0644 packaging/udev/99-uinput.rules /etc/udev/rules.d/99-uinput.rules
sudo modprobe uinput
sudo usermod -aG input "$USER"        # log out and back in afterwards
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Without this the daemon still starts, but input injection falls back to the logging mock.

## 3. Install and enable the user unit

The daemon runs as a systemd **user** service: it starts with your graphical login, stops when
you log out, and `Restart=on-failure` brings it back 2 seconds after a crash. A daemon started by
hand in a terminal does none of that, so a reboot or a crash leaves the headset black.

### With `service install` (any build)

`linux-quest-host service install` writes `~/.config/systemd/user/linux-quest-host.service` with
`ExecStart` pointing at the binary you run it from, and reloads systemd. Daemon options go after
`--`. It does not enable or start anything unless asked:

```bash
linux-quest-host service install -- --streams 3      # write the unit and reload, nothing else
linux-quest-host service install --enable --start -- --streams 3   # also start it now and at login
linux-quest-host service install --tray --enable       # also install the tray icon unit
linux-quest-host service print -- --streams 3        # show what would be written
linux-quest-host service uninstall                   # disable, stop and remove both units
```

- Install the binary somewhere stable first (`cargo install --path crates/host --root ~/.local`).
  A binary in a cargo `target` directory works, but `cargo clean` deletes it and the service then
  fails to start; `install` prints a note when that is the case.
- Stop a daemon you started by hand before `--start` (or `systemctl --user start`): it holds the
  PID lock, so the service would fail. `install --start` refuses while such a daemon runs.
- `install` refuses to replace a unit file that has different contents (for example other daemon
  options). Pass `--force` to replace it.
- The portal screen-share grant is kept in `~/.local/state/linux-quest/screencast-restore-token`,
  so a service restart does not show GNOME's share dialog again.

### By hand (packages ship the unit in `/usr/lib/systemd/user`)

```bash
mkdir -p ~/.config/systemd/user
cp packaging/systemd/linux-quest-host.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now linux-quest-host
```

The unit is `WantedBy=graphical-session.target`, so it starts at login and stops when the graphical session ends. It starts after `pipewire.service`.

## 4. Operate it

```bash
systemctl --user status linux-quest-host     # state and recent log lines
journalctl --user -u linux-quest-host -f     # live logs
systemctl --user restart linux-quest-host
systemctl --user disable --now linux-quest-host
```

`linux-quest-host dashboard` still works against the running service.

## Shutdown and restart behaviour

- `systemctl --user stop` sends `SIGTERM`. The daemon treats it like Ctrl-C: it stops the IPC server, releases held keys and buttons, and removes its socket and PID file. It must stop within 10 seconds (`TimeoutStopSec`), or systemd sends `SIGKILL`.
- `Restart=on-failure` restarts the daemon after 2 seconds if it exits with an error. A clean stop does not restart it.
- If the daemon keeps failing (for example, another instance holds the PID lock), systemd gives up after 5 starts within 60 seconds. Check `journalctl --user -u linux-quest-host`, then `systemctl --user reset-failed linux-quest-host` once fixed.

## Tray icon

`linux-quest-host tray` puts a status icon in the system tray. The penguin logo has a cyan badge for USB, blue for Wi-Fi, amber when no headset is connected, and a grey ring when the daemon is not running. Disconnected and offline logos are desaturated. Its menu has:

- **Status**: the connection, and below it what is streaming and at what bitrate (or that it is paused), as disabled rows.
- **Pause Streaming / Resume Streaming**: stops or restarts video to the headset. Capture keeps running, so resuming is immediate. Shown only by a daemon new enough to report it.
- **Choose Screen…**: shows GNOME's screen-sharing dialog so you can pick what the headset's main screen shows. Disabled while the daemon streams a test pattern, and while the dialog is open.
- **Open Linux Quest**: starts the desktop app (`linux-quest`), the one next to `linux-quest-host` if there is one, else the one on `PATH`. Left-click does the same.
- **Open dashboard**: runs `linux-quest-host dashboard` in a terminal. The terminal is `$TERMINAL`, or `x-terminal-emulator` if that is unset. It must accept `-e <command>`.
- **Restart service**: runs `systemctl --user --no-block restart linux-quest-host`.
- **Quit**: exits the tray. The daemon keeps running.

The tray is a separate process. It asks the daemon for its status over the IPC socket every 2 seconds (`--interval-ms`, minimum 500). It uses `ksni`, a pure Rust D-Bus implementation, so it is in every build; the `tray` cargo feature is kept and does nothing.

**It needs a StatusNotifierItem host.** KDE Plasma has one built in. GNOME needs the AppIndicator extension (on Ubuntu, `gnome-extensions enable ubuntu-appindicators@ubuntu.com`). Other desktops need a bar with a tray module, such as waybar's `tray`. With no host running, the tray logs one warning and keeps running. It shows the icon as soon as a host starts.

**Ordering with the daemon unit.**

- The tray does not depend on the daemon. While the daemon is down it shows "Daemon not running", and it recovers on its own once the daemon starts. Start order does not affect correctness.
- `Restart service` restarts only the daemon, and the tray keeps running because it is a separate process. This is also why the tray is not built into `daemon`.
- To start the tray with your session, install [`systemd/linux-quest-tray.service`](systemd/linux-quest-tray.service) next to the daemon's unit (the packages ship both), or run `linux-quest-host service install --tray`. `After=` only sets the start order. It does not start the daemon.

```bash
linux-quest-host service install --tray --enable --start
```

The tray needs the graphical session environment (`DISPLAY` or `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, and the D-Bus session bus) to open a terminal or the app and to reach the StatusNotifier host. Many desktops import these into the systemd user manager at login. If yours does not, run this from your session startup (for example an autostart entry) before enabling the unit, or set the variables in the unit with `Environment=`:

```bash
systemctl --user import-environment DISPLAY WAYLAND_DISPLAY XDG_RUNTIME_DIR
```

Choosing Quit exits with status 0, so `Restart=on-failure` does not start the tray again.

**Link status.** The tray shows what the daemon reports: the transport (USB or Wi-Fi) of the connected client, and whether video goes to it. With no client connected the tray shows "Disconnected".

## Verify the unit file

```bash
systemd-analyze --user verify packaging/systemd/linux-quest-host.service packaging/systemd/linux-quest-tray.service
```

`verify` warns if `/usr/bin/linux-quest-host` does not exist yet. That is expected before installing the binary.

## Build packages locally

All commands below run from the repository root. They only create local files; CI
uploads build results as temporary workflow artifacts. This project does not
publish packages or create releases.

### Debian package

Install Rust and the native build prerequisites, then run:

```bash
cargo install --locked cargo-deb
cargo build --release --locked -p linux-quest-host --features mdns
cargo deb -p linux-quest-host --no-build
```

The host `.deb` is written under `target/debian/`. It installs the daemon, `/dev/uinput`
udev rule, boot-time module configuration, systemd user unit and licenses.

### RPM package

```bash
cargo install --locked cargo-generate-rpm
cargo build --release --locked -p linux-quest-host --features mdns
cargo generate-rpm -p crates/host
```

The `.rpm` is written under `target/generate-rpm/` and contains the same host
files. The package runs a best-effort udev reload after installation.

### Arch Linux / AUR package

On Arch Linux, install `base-devel` and `rust`. For a published source tag, update
`pkgver` and the source checksum in `packaging/arch/PKGBUILD`, then build it:

```bash
cd packaging/arch
makepkg --syncdeps --cleanbuild
```

This builds the host binary with mDNS and installs the udev rule, module-load
configuration, user unit and licenses. The recipe does not upload to the AUR.

### Flatpak

Install `flatpak`, `flatpak-builder`, Python 3 with `aiohttp`, `PyYAML` and `tomlkit`,
and the Flathub `org.freedesktop.Sdk//24.08`, `org.freedesktop.Platform//24.08` and
`org.freedesktop.Sdk.Extension.rust-stable//24.08` refs. Generate the locked Cargo
dependency source manifest and build locally:

```bash
python3 packaging/flatpak/flatpak-cargo-generator.py Cargo.lock \
  -o packaging/flatpak/cargo-sources.json
flatpak-builder --force-clean --repo=repo build-dir packaging/flatpak/org.linuxquest.Host.yml
```

Install the local build for testing with `flatpak remote-add --user
--no-gpg-verify linux-quest-local repo` and `flatpak install --user
linux-quest-local org.linuxquest.Host`; remove it with `flatpak uninstall --user
org.linuxquest.Host`.
Flatpak runs the host daemon in a sandbox. Configure its device and network access
for your desktop before expecting `/dev/uinput` or capture devices to be available;
the udev rule and systemd unit are shipped as docs for host-side setup.

### Meta Quest APK

Install Android SDK platform 34, build-tools 34.0.0, NDK 27.2.12479018, Java 17,
Rust target `aarch64-linux-android`, and `cargo-ndk`. Then build a debug-signed,
sideloadable APK:

```bash
cargo install --locked cargo-ndk
rustup target add aarch64-linux-android
export ANDROID_NDK_HOME="$ANDROID_SDK_ROOT/ndk/27.2.12479018"
bash crates/client/android/build.sh --release
```

The APK is `crates/client/target/android-apk/linux-quest-release.apk`. It is
debug-signed for sideload testing, not signed for store distribution. Install it
on a connected Quest with `scripts/install-quest.sh` (or pass the APK path).

## Quest sideloading

The `.deb` and `.rpm` run `udevadm control --reload` and `udevadm trigger` after
install (best effort). Neither loads `uinput` immediately; the module-load config
does that at the next boot. The AUR package has no maintainer scripts, so reload
udev manually after installation. Its optional dependencies cover clipboard
tools, PipeWire, and NVIDIA encoding.

### Sideloading the APK with `scripts/install-quest.sh`

With a Quest connected over USB and Developer Mode on:

```bash
scripts/install-quest.sh                       # newest APK in crates/client/target/android-apk/
scripts/install-quest.sh ~/Downloads/linux-quest-v0.1.0-debugsigned.apk
ANDROID_SERIAL=<serial> scripts/install-quest.sh   # pick one device when several are connected
```

With no path argument, the script installs the newer of `linux-quest-release.apk` and `linux-quest-debug.apk` and warns when both exist. It needs `adb` on `PATH` and accepts the headset's "Allow USB debugging" prompt.

### Testing LAN discovery on a real Quest (no IP address)

The host package recipes and Quest APK use the `mdns` feature. A source build needs
it explicitly: `cargo build --release -p linux-quest-host --features mdns` and
`crates/client/android/build.sh` (which already passes `--features mdns`).

1. Put the PC and the Quest on the same Wi-Fi network (same subnet; guest/isolated SSIDs and some mesh "AP isolation" settings block multicast). Allow UDP `5353` (mDNS), TCP `48444`, UDP `48442` and UDP `48440` through the PC firewall, for example `sudo ufw allow from 192.168.1.0/24`.
2. Start the host with `linux-quest-host daemon` (do not pass `--no-advertise`). The log shows `Advertising host on the LAN` with the instance name, which is the PC's short hostname.
3. Verify the PC is visible to the LAN, from a third machine: `avahi-browse -rt _linuxquest._tcp` should list the instance with TXT `proto=1`, `input=48442`, `codecs=av1,hevc`, and `video=48440` while a headset is streaming.
4. On the Quest make sure no host override is set: `adb shell setprop debug.linuxquest.host ""` and `adb shell setprop debug.linuxquest.transport ""`, and that no `adb reverse` is active (`adb reverse --remove-all`, or unplug USB). Then launch the app.
5. Watch `adb logcat -s linux_quest_client:* RustStdoutStderr:*`. Expected lines: `LAN discovery selected a host`, then `connecting to discovered host` with the PC's LAN address. Video should start without any IP having been entered.

Troubleshooting:

- `no host found on the LAN`: the browse ran for about four seconds and nothing answered. Check steps 1 and 3, then relaunch the app (discovery runs at launch).
- `LAN discovery unavailable`: the Quest's mDNS daemon could not start. Set the host by hand: `adb shell setprop debug.linuxquest.host 192.168.1.20`.
- Several hosts on the LAN: the client prefers one that is already streaming video, then the first by name, and logs how many others it saw. Pin one with `debug.linuxquest.host`.
- With a USB tunnel up (`adb reverse tcp:48444 tcp:48444`), discovery is skipped and the cable is used.

SSDP/UPnP is not implemented; mDNS is the only discovery protocol.

## Complete Ubuntu app packages and release staging

Use `scripts/ci/build-ubuntu-packages.sh` with the native dependencies in
`packaging/ubuntu/Dockerfile` to build both the GUI and daemon packages. The GUI
package includes the application launcher and penguin icon. For a user-local
source install with a desktop shortcut, run `scripts/install-desktop.sh
--desktop-shortcut --tray`. The tray starts automatically at your next login;
Ubuntu needs its AppIndicators extension enabled to show it in the top bar. The app uses blue/cyan accents from the logo while retaining
native light/dark mode. See [publishing.md](../docs/publishing.md) for UECB builds,
draft releases and the remaining Meta/Ubuntu store requirements.
