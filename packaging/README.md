# Packaging

Files for running `linux-quest-host` as a background service on Ubuntu / systemd desktops.

| Path | Purpose |
| :--- | :--- |
| `systemd/linux-quest-host.service` | systemd **user** unit running `linux-quest-host daemon` |
| `udev/99-uinput.rules` | Grants the `input` group access to `/dev/uinput` |

The tray icon is not included yet (tracked in #32).

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

## Verify the unit file

```bash
systemd-analyze --user verify packaging/systemd/linux-quest-host.service
```

`verify` warns if `/usr/bin/linux-quest-host` does not exist yet. That is expected before installing the binary.
