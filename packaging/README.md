# Packaging

Files for running `linux-quest-host` as a background service on Ubuntu / systemd desktops.

| Path | Purpose |
| :--- | :--- |
| `systemd/linux-quest-host.service` | systemd **user** unit running `linux-quest-host daemon` |
| `udev/99-uinput.rules` | Grants the `input` group access to `/dev/uinput` |
| `modules-load/linux-quest.conf` | Loads the `uinput` module at boot |
| `debian/postinst`, `debian/copyright` | Debian maintainer script (reloads udev) and copyright file, used by `cargo deb` |
| `arch/PKGBUILD` | AUR package recipe (see below) |

The optional tray icon (`linux-quest-host tray`, tracked in #32) is off by default. See [Optional tray icon](#optional-tray-icon) below.

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

## Optional tray icon

`linux-quest-host tray` puts a status icon in the system tray. The icon is green for USB, blue for Wi-Fi, grey when no headset is streaming, and a hollow ring when the daemon is not running. Its menu has:

- **Status**: the connection state, as the first (disabled) row.
- **Open dashboard**: runs `linux-quest-host dashboard` in a terminal. Left-click does the same. The terminal is `$TERMINAL`, or `x-terminal-emulator` if that is unset. It must accept `-e <command>`.
- **Restart service**: runs `systemctl --user --no-block restart linux-quest-host`.
- **Quit**: exits the tray. The daemon keeps running.

The tray is a separate process. It asks the daemon for its status over the IPC socket every 2 seconds (`--interval-ms`, minimum 500).

**Build it with the `tray` feature.** The feature pulls in D-Bus (`ksni`), so it is off by default. The release packages do not include it:

```bash
cargo install --path crates/host --bin linux-quest-host --features tray --root ~/.local
```

Without the feature, `linux-quest-host tray` prints this command and exits with status 2.

**It needs a StatusNotifierItem host.** KDE Plasma has one built in. GNOME needs the AppIndicator extension. Other desktops need a bar with a tray module, such as waybar's `tray`. With no host running, the tray logs one warning and keeps running. It shows the icon as soon as a host starts.

**Ordering with the daemon unit.**

- The tray does not depend on the daemon. While the daemon is down it shows "Daemon not running", and it recovers on its own once the daemon starts. Start order does not affect correctness.
- The daemon unit is unchanged. `Restart service` restarts only the daemon, and the tray keeps running because it is a separate process. This is also why the tray is not built into `daemon`.
- To start the tray with your session, install a user unit after the daemon's. The packages do not ship one. `After=` only sets the start order. It does not start the daemon.

```ini
# ~/.config/systemd/user/linux-quest-tray.service
[Unit]
Description=Linux Quest host tray icon
PartOf=graphical-session.target
After=graphical-session.target linux-quest-host.service

[Service]
Type=simple
ExecStart=%h/.local/bin/linux-quest-host tray
# systemd user services do not inherit your shell's TERMINAL. Uncomment and set one if you want a specific terminal:
# Environment=TERMINAL=kitty
Restart=on-failure
RestartSec=5

[Install]
WantedBy=graphical-session.target
```

The tray needs the graphical session environment (`DISPLAY` or `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, and the D-Bus session bus) to open a terminal and reach the StatusNotifier host. Many desktops import these into the systemd user manager at login. If yours does not, run this from your session startup (for example an autostart entry) before enabling the unit, or set the variables in the unit with `Environment=`:

```bash
systemctl --user import-environment DISPLAY WAYLAND_DISPLAY XDG_RUNTIME_DIR
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now linux-quest-tray
```

Choosing Quit exits with status 0, so `Restart=on-failure` does not start the tray again.

**Link status.** The tray shows what the daemon reports. The daemon's status snapshot carries the real link: the transport (USB or Wi-Fi) of the connected client, and `streaming` only while a client is connected and a virtual display exists. With no client connected the tray shows "Disconnected".

## Verify the unit file

```bash
systemd-analyze --user verify packaging/systemd/linux-quest-host.service
```

`verify` warns if `/usr/bin/linux-quest-host` does not exist yet. That is expected before installing the binary.

## Packages and the Quest sideload script

Release builds (see `.github/workflows/release.yml`) attach these to each GitHub Release:

| Artifact | Built with | Installs |
| :--- | :--- | :--- |
| `linux-quest-host_<ver>-1_amd64.deb` | `cargo deb -p linux-quest-host --no-build` | host binary, udev rule, `uinput` modules-load config, user unit, licenses |
| `linux-quest-host-<ver>-1.x86_64.rpm` | `cargo generate-rpm -p crates/host` | same files as the `.deb` |
| `linux-quest-host-<tag>-x86_64-linux.tar.gz` | plain tar of the binary | binary, udev rule, licenses (copy the rest by hand) |
| `linux-quest-<tag>-debugsigned.apk` | `crates/client/android/build.sh --release` | Quest client APK, debug-signed |

The `.deb` and `.rpm` run `udevadm control --reload` and `udevadm trigger` after install (best effort), so the uinput rule applies without a reboot. Neither loads the `uinput` module immediately; the modules-load config does that at the next boot.

The AUR package (`arch/PKGBUILD`) builds from the tagged source tarball. It installs the binary, udev rule, modules-load config, user unit and licenses, but no maintainer scripts, so run the `udevadm` reload yourself after install. Its `optdepends` list covers clipboard tools, PipeWire, and `nvidia-utils` for NVENC. Its `check()` runs only the unit tests (`--lib`).

The release APK is signed with the debug keystore that `build.sh` generates. It is installable by sideloading, but it is not production-signed. Releases stay marked as pre-releases until a release keystore is configured.

### Sideloading the APK with `scripts/install-quest.sh`

With a Quest connected over USB and Developer Mode on:

```bash
scripts/install-quest.sh                       # newest APK in crates/client/target/android-apk/
scripts/install-quest.sh ~/Downloads/linux-quest-v0.1.0-debugsigned.apk
ANDROID_SERIAL=<serial> scripts/install-quest.sh   # pick one device when several are connected
```

With no path argument, the script installs the newer of `linux-quest-release.apk` and `linux-quest-debug.apk` and warns when both exist. It needs `adb` on `PATH` and accepts the headset's "Allow USB debugging" prompt.

### Testing LAN discovery on a real Quest (no IP address)

The release `.deb`/`.rpm`/AUR package and the release APK are built with the `mdns` feature. A source build needs it explicitly: `cargo build --release -p linux-quest-host --features mdns` and `crates/client/android/build.sh` (which already passes `--features mdns`).

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
