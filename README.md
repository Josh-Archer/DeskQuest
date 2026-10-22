# linux-quest

**High-performance, ultra-low latency spatial multi-monitor desktop streamer from Linux (Ubuntu) to Meta Quest — written entirely in Rust.**

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20or%20Apache--2.0-blue.svg)](LICENSE)
[![Language: Rust](https://img.shields.io/badge/Language-Rust-orange.svg)](https://www.rust-lang.org/)
[![Target: Meta Quest 3 / Pro](https://img.shields.io/badge/Target-Meta%20Quest%203%20%2F%20Pro-purple.svg)](https://www.meta.com/quest/)
[![Host: Linux / Ubuntu](https://img.shields.io/badge/Host-Linux%20%2F%20Ubuntu-E95420.svg)](https://ubuntu.com/)

---

## 🎯 The Vision

[Immersed](https://immersed.com) and similar commercial tools demonstrated that working with virtual multi-monitors in VR is transformative for developers and power users. However, existing solutions on Linux suffer from:
- Proprietary, closed-source codebases
- Increasing bloat and telemetry
- High latency and inconsistent frame pacing
- Sub-par text clarity due to intermediate 3D scene rendering passes
- Fragile virtual display setups on modern Linux compositors (Wayland/X11)

**`linux-quest`** is built from scratch in **Rust** to deliver an open-source, bloat-free, hardware-accelerated, and uncompromisingly fast spatial desktop environment.

### Primary Goals
1. **Ultra-Low Latency:** Target **< 10ms motion-to-photon latency over USB (ADB reverse tethering)** and **< 15ms over 5GHz/6GHz Wi-Fi (Wi-Fi 6/6E)**.
2. **Sub-Pixel Text Crispness:** Render desktop streams directly via **OpenXR Composition Layers (Quad & Cylinder layers)**, bypassing intermediate scene redraws so text in IDEs and terminals is crystal clear.
3. **True Multi-Monitor on Linux:** Seamless creation and management of virtual headless displays on Wayland (`wlroots`, Mutter) and X11 without physical dummy plugs.
4. **Zero-Copy Pipeline:** Frame capture (PipeWire / DMA-BUF / NVFBC) ➔ Hardware Encoder (NVENC / VA-API) ➔ Network Transport (QUIC / UDP) ➔ Hardware Decoder (`AMediaCodec`) ➔ OpenXR Swapchain.
5. **Spatial Comfort & Ergonomics:** High-definition Passthrough support (`XR_FB_passthrough`), customizable curved virtual screens, 6DoF repositioning, and seamless Linux mouse/keyboard injection (`uinput`).

---

## 🏗️ Architecture Overview

```
+-----------------------------------------------------------------------------------------+
|                                    LINUX HOST (Rust)                                    |
|                                                                                         |
|  +--------------------+    +--------------------+    +-------------------------------+  |
|  | Virtual Displays   |    | Zero-Copy Capture  |    | Hardware Encoder              |  |
|  | (Wayland / DRM)    |--->| PipeWire / DMA-BUF |--->| NVENC / VA-API (AV1 / HEVC)   |  |
|  +--------------------+    +--------------------+    +---------------+---------------+  |
|                                                                      |                  |
|  +--------------------+    +--------------------+                    v                  |
|  | Input Injection    |    | Audio Capture      |        +-----------------------+      |
|  | (uinput / libinput)|<---| PipeWire (Opus)    |        | Low-Latency Transport |      |
|  +--------------------+    +--------------------+        | QUIC / Custom UDP /   |      |
|                                                          | USB Tethering (ADB)   |      |
+----------------------------------------------------------------------|------------------+
                                                                       | Network / USB
+----------------------------------------------------------------------|------------------+
|                                  META QUEST 3 CLIENT                 v                  |
|                                                                                         |
|  +-----------------------------------+        +--------------------------------------+  |
|  | Low-Latency Receiver & Jitter Buf |------->| AMediaCodec Hardware Decoder         |  |
|  +-----------------------------------+        | Zero-copy decode to AHardwareBuffer  |  |
|                                               +-------------------+------------------+  |
|                                                                   |                     |
|  +-----------------------------------+                            v                     |
|  | Spatial Tracking & Passthrough    |        +--------------------------------------+  |
|  | 6DoF, XR_FB_passthrough, Hands    |------->| OpenXR Composition Layer Engine      |  |
|  +-----------------------------------+        | Direct Quad/Cylinder Layer Blitting  |  |
|                                               +--------------------------------------+  |
+-----------------------------------------------------------------------------------------+
```

For an in-depth breakdown of the technical components, see [ARCHITECTURE.md](ARCHITECTURE.md).

---

## ⚡ Tech Stack

| Component | Technology | Rationale |
| :--- | :--- | :--- |
| **Language** | **Rust** | Zero-cost abstractions, memory safety, deterministic latency (no GC pauses), rich systems ecosystem. |
| **Linux Display Capture** | **PipeWire / DMA-BUF / NVFBC** | Zero-copy GPU memory access on modern Wayland & X11 compositors. |
| **Hardware Video Encoding** | **NVENC & VA-API** (AV1, HEVC, H.264) | Hardware accelerated; RTX 40/50-series dual AV1 NVENC with ultra-low latency presets (P1/P2, intra-refresh). |
| **Video Transport** | **QUIC / Custom UDP / USB (ADB)** | Sub-millisecond packet pacing, FEC (Forward Error Correction), and USB reverse-tethering support. |
| **VR Runtime** | **OpenXR (Android / Horizon OS)** | Industry standard cross-vendor XR runtime with native Meta extensions for Passthrough and Composition Layers. |
| **Quest Client Core** | **Rust (`openxr` + `ash` Vulkan) / Android NDK** | Direct access to native Vulkan swapchains and `AMediaCodec` hardware decoding buffers. |
| **Audio Streaming** | **PipeWire + Opus + Android AAudio** | Sub-3ms host capture/encode (Opus 2.5ms frames), adaptive jitter buffer, PLC/FEC loss concealment, A/V presentation sync, and native AAudio low-latency sink. |
| **Input Redirection** | **Linux `uinput`** | Kernel-level low-overhead virtual keyboard and mouse emulation. |
| **Management Dashboard** | **Ratatui + Crossterm** | Real-time terminal status dashboard for active virtual displays, latency waterfall, client link, and audio stream stats. |

---

## 🖥️ Host Status Dashboard (TUI)

Launch the real-time terminal status dashboard powered by `ratatui`:

```bash
# Live dashboard connecting to the running background daemon
linux-quest-host dashboard

# Standalone simulation / demo mode with synthetic telemetry
linux-quest-host dashboard --demo

# Run background daemon holding virtual monitors with interactive dashboard
linux-quest-host daemon --tui
```

Hotkeys:
- `q` or `Esc`: Exit dashboard (restores terminal cleanly)
- `r`: Reset rolling telemetry history
- `Space`: Pause / Resume real-time updates

---

## 🪟 Desktop App

`linux-quest` is a GTK 4 / libadwaita app for GNOME. It talks only to the running daemon, over its IPC socket:

- **Status**: whether the headset is connected (USB or Wi-Fi), the frame rate, latency, bitrate and packet loss, and a switch to pause the video.
- **Screens**: the headset's screens seen from above, arranged around you. Drag a screen to move it around you, drag an end to resize it, scroll to scale it, or set its direction, distance, height, tilt, width and curve exactly. The headset applies each change as you drag. The host's virtual monitors can be added, removed, or given another resolution and refresh rate here too.
- **Quest Settings**: bitrate (adaptive or fixed), preferred codec, refresh rate, curved or flat screens, the performance overlay and passthrough.
- **Choose Screen…** shows GNOME's screen-sharing dialog so you can pick which screen or window the headset's main screen shows.

Layout and settings need the headset app running and connected to the host's control port: TCP 48444 over the LAN, or over USB through `adb reverse tcp:48445 tcp:48444`, which the daemon sets up when it manages the tunnel (with `--no-adb`, run that `adb reverse` yourself). They also need a daemon and headset app from this version; an older daemon is never contacted over USB.

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev     # build dependencies (Fedora: gtk4-devel libadwaita-devel)
cargo install --path crates/desktop --root ~/.local
install -Dm644 packaging/desktop/org.linuxquest.Desktop.desktop ~/.local/share/applications/org.linuxquest.Desktop.desktop
linux-quest
```

To try it without a headset, run the demo daemon (the real IPC server with mock hardware and a simulated headset; it binds no fixed port and never opens the screen-sharing dialog):

```bash
export LINUX_QUEST_RUNTIME_DIR=$(mktemp -d)
cargo run -p linux-quest-desktop --example demo_daemon &
cargo run -p linux-quest-desktop
```

---

## 🔁 Headless Background Service (systemd)

To run the daemon at login without a terminal, and have it restarted if it crashes, install it as a systemd user service. `service install` writes the unit for the binary you run it from, with the daemon options after `--`:

```bash
linux-quest-host service install --enable --start -- --streams 3
```

Or install the unit in [`packaging/systemd/linux-quest-host.service`](packaging/systemd/linux-quest-host.service) by hand:

```bash
mkdir -p ~/.config/systemd/user
cp packaging/systemd/linux-quest-host.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now linux-quest-host
```

See [`packaging/README.md`](packaging/README.md) for installation, the `/dev/uinput` udev rule and operating notes.

### Build and install packages

Packaging is currently for local builds and CI test artifacts; no package
repository or app store is being published. Build instructions for `.deb`, `.rpm`,
Arch/AUR, Flatpak and the Quest APK are in [`packaging/README.md`](packaging/README.md#build-packages-locally).
Install a locally built host package with `sudo apt install ./linux-quest-host_*.deb`
on Debian/Ubuntu or `sudo dnf install ./linux-quest-host-*.rpm` on Fedora. On Arch,
build and install the PKGBUILD with `makepkg --syncdeps --cleanbuild --install`.
Install a locally built Flatpak bundle with `flatpak install --user ./linux-quest-host.flatpak`.
The APK is debug-signed for sideloading; connect a Quest with Developer Mode enabled
and run `scripts/install-quest.sh <path-to-apk>`.

### Tray icon

`linux-quest-host tray` shows the headset's status in the system tray: how it is connected and what is streaming, with Pause/Resume Streaming, Choose Screen…, Open Linux Quest (the desktop app; also a left click), Open Dashboard, Restart Service and Quit. It is part of every build and runs as its own process:

```bash
linux-quest-host tray
linux-quest-host service install --tray --enable --start   # or start it with every login
```

It needs a StatusNotifierItem host: KDE Plasma, a bar with a tray module, or on GNOME the AppIndicator extension (`gnome-extensions enable ubuntu-appindicators@ubuntu.com` on Ubuntu). It can start before or after the daemon. Details are in [`packaging/README.md`](packaging/README.md#tray-icon).

---

## 🎮 Kernel Input Injection (`/dev/uinput` Setup)

`linux-quest` uses the Linux kernel's `uinput` subsystem to inject input events with native kernel-level responsiveness:
- **Virtual Mouse:** Relative movement (`REL_X`, `REL_Y`), discrete & high-resolution scrolling (`REL_WHEEL`, `REL_HWHEEL`, `REL_WHEEL_HI_RES`, `REL_HWHEEL_HI_RES`), and mouse buttons (`BTN_LEFT`, `BTN_RIGHT`, `BTN_MIDDLE`, `BTN_SIDE`, `BTN_EXTRA`).
- **Virtual Absolute Pointer:** Absolute coordinate positioning (`ABS_X`, `ABS_Y`) and pointer buttons for spatial ray pointing.
- **Virtual Keyboard:** Full evdev keycodes with autorepeat support and automatic held key/button release on client disconnect or drop.
- **Committed text:** Horizon soft keyboard (toggle with `Super+K`) and IME commits carry UTF-8 text; install `wtype` on Wayland or `xdotool` on X11 to inject non-ASCII commits.
- **Keyboard focus:** `Ctrl+Alt+Escape` hides the Horizon keyboard and returns focus to the headset UI. Set Android property `debug.linuxquest.keyboard_escape_chord` (or `LINUX_QUEST_KEYBOARD_ESCAPE_CHORD` when available) to a chord such as `META+BACK` to change it. Alt+Tab and Ctrl+Alt+F1 through F12 are left to Horizon OS when it exposes them to the app.

### Permissions & Udev Configuration
To allow `linux-quest-host` to create virtual devices without running as `root`:

1. **Load the `uinput` kernel module:**
   ```bash
   sudo modprobe uinput
   ```

2. **Install the udev rule:**
   Create `/etc/udev/rules.d/99-uinput.rules`:
   ```udev
   KERNEL=="uinput", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"
   ```

3. **Add your user to the `input` group:**
   ```bash
   sudo usermod -aG input $USER
   ```
   *(Log out and log back in for the new group membership to take effect).*

4. **Reload udev rules:**
   ```bash
   sudo udevadm control --reload-rules && sudo udevadm trigger
   ```

When `/dev/uinput` is unavailable or permissions are not yet configured, the host daemon automatically falls back to an in-memory/logging mock injector and logs actionable guidance.

---

## 📋 Bidirectional Clipboard Synchronization

`linux-quest` provides seamless, bidirectional UTF-8 text clipboard synchronization between Linux desktop and Meta Quest headsets:
- **Host Desktop Integration:** Automatically detects Wayland (`wl-copy`/`wl-paste`) vs X11 (`xclip`/`xsel`) via `WAYLAND_DISPLAY`/`DISPLAY`, polling with debounced change detection.
- **Quest Client Integration:** `ClipboardManager` trait wrapping Android Horizon OS clipboard services (JNI) with headless mock fallbacks.
- **Loop-Free Synchronization:** Origin tracking and monotonic versions resolve race conditions via deterministic last-writer ordering while suppressing echo loops.
- **Large Text Chunking:** Safe slicing on UTF-8 character boundaries for payloads up to 4 MiB carried over the reliable control channel.

## 🔒 QUIC Control Channel

The transport crate has an optional QUIC control channel (`linux-quest-transport/quic`). It carries the existing framed control messages, including handshakes, heartbeat/telemetry and chunked clipboard data, over one reliable bidirectional stream. QUIC is opt-in so default workspace builds do not add QUIC/TLS dependencies; it requires no system libraries.

The host and client authenticate each other with TLS 1.3 certificates pinned by SHA-256 fingerprint. The application must provision and persist the host/client identities and exchange fingerprints over a trusted channel; this transport crate does not provide pairing or a trust store. Do not accept an authentication failure as a reason to downgrade. `ControlClientSession::connect_with_tcp_fallback` falls back to the existing TCP control channel only for connection failures and supports reconnecting the selected transport. The caller must run both the `QuicControlServer` and existing TCP listener for fallback. The host daemon and Quest client still select TCP directly until identity provisioning and pairing are wired into their configuration.

Run the QUIC loopback coverage with `cargo test -p linux-quest-transport --features quic --test quic_control_loopback`. The ignored release benchmark compares sequential loopback heartbeat round trips with QUIC and TCP; it measures framing/transport overhead on one machine, not Wi-Fi or headset latency. One release run on the development host measured TCP p50/p99 at 5.6/6.5 µs and QUIC at 14.1/21.8 µs (5,000 warmed samples each). This shows about 8.5 µs additional median loopback RTT for QUIC; network conditions and TLS setup will change real-session results. Run it with `cargo test -p linux-quest-transport --release --features quic --test quic_control_loopback -- --ignored --nocapture quic_vs_tcp`.

---

## 🗺️ Roadmap & Milestones

The project is structured into 7 core phases. Status reflects what is merged on `main`; see [ROADMAP.md](ROADMAP.md) for details.

- [x] **Phase 1:** Foundation & Architecture (Cargo workspace, zero-copy binary protocol, telemetry, CI)
- [~] **Phase 2:** Linux Capture & Hardware Encoding Engine (NVENC and feature-gated VA-API backends; VA-API HEVC and H.264, experimental AV1 opt-in, DMA-BUF import, CBR, intra-refresh and reference picture invalidation are implemented and mock-tested, but AMD/Intel hardware validation remains manual; see [`docs/vaapi-manual-test.md`](docs/vaapi-manual-test.md). VA-API is selected with `LINUX_QUEST_VIDEO_ENCODER=vaapi` or `linux-quest-host daemon --encoder vaapi`; automatic selection tries NVENC then VA-API.)
- [~] **Phase 3:** Low-Latency Network & USB Transport (paced UDP, Reed-Solomon/XOR FEC, ADB reverse tunnel and optional QUIC control channel done; mDNS LAN discovery with Quest auto-connect done, SSDP not implemented; QUIC identity pairing is not yet wired into the host/client application configuration)
- [x] **Phase 4:** Meta Quest OpenXR Client & Hardware Decoder (AMediaCodec, Vulkan, Composition Layers)
- [~] **Phase 5:** Virtual Multi-Monitor Management on Linux (wlroots, Mutter and X11 backends with dynamic EDID done; EVDI kernel dummy-driver fallback implemented but never run against real EVDI, see #31)
- [x] **Phase 6:** Spatial Workspace, Passthrough & Input Redirection (6DoF screens, passthrough, controller/hand manipulation, uinput injection, clipboard sync)
- [~] **Phase 7:** Audio Streaming & Production Polish (Opus audio with A/V sync and Ratatui dashboard done; tray icon/systemd service and packaging pending)

> **Note:** Much of the code is tested against mock backends (display, decoder, uinput, clipboard, audio sink). End-to-end validation on real Quest 3 and GPU hardware, and the latency/CPU targets above, are still to be confirmed.

Detailed milestone descriptions, deliverables, and acceptance criteria are outlined in [ROADMAP.md](ROADMAP.md).

### VA-API hardware encoding (AMD and Intel)

The encoder crate enables its `vaapi` feature by default. VA-API is loaded at runtime, so normal workspace builds do not need libva development headers or an installed GPU driver. To omit the backend and its runtime probe, build with `--no-default-features` for `linux-quest-encoder`. The host exposes the feature as `vaapi` and uses runtime capability probing; automatic selection tries NVENC first, then VA-API. VA-API requires a driver that supports CBR and the requested codec/profile. HEVC and H.264 are supported; AV1 remains experimental and disabled unless `LINUX_QUEST_VAAPI_AV1=1` is set.

The backend imports supported NV12 and RGB DMA-BUFs through VA-API PRIME. NV12 imports directly; RGB requires the driver's video processing entrypoint for zero-copy conversion. CPU-backed frames remain available as a fallback. Mocked capability, selection, import, rate-control, intra-refresh and reference-invalidation tests do not require a GPU.

Follow the [AMD and Intel manual validation procedure](docs/vaapi-manual-test.md) before treating real-device support as verified.

---

## 📋 Tracking Progress

All tasks and feature implementations are tracked as GitHub Issues:
👉 **[View the Project Issue Board](https://github.com/Josh-Archer/linux-quest/issues)**

---

## 📄 License

Dual-licensed under either:
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
