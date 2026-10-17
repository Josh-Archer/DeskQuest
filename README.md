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

## 🔁 Headless Background Service (systemd)

To run the daemon at login without a terminal, install the systemd user unit in [`packaging/systemd/linux-quest-host.service`](packaging/systemd/linux-quest-host.service):

```bash
mkdir -p ~/.config/systemd/user
cp packaging/systemd/linux-quest-host.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now linux-quest-host
```

See [`packaging/README.md`](packaging/README.md) for installation, the `/dev/uinput` udev rule and operating notes. The tray icon is not included yet (tracked in #32).

---

## 🎮 Kernel Input Injection (`/dev/uinput` Setup)

`linux-quest` uses the Linux kernel's `uinput` subsystem to inject input events with native kernel-level responsiveness:
- **Virtual Mouse:** Relative movement (`REL_X`, `REL_Y`), discrete & high-resolution scrolling (`REL_WHEEL`, `REL_HWHEEL`, `REL_WHEEL_HI_RES`, `REL_HWHEEL_HI_RES`), and mouse buttons (`BTN_LEFT`, `BTN_RIGHT`, `BTN_MIDDLE`, `BTN_SIDE`, `BTN_EXTRA`).
- **Virtual Absolute Pointer:** Absolute coordinate positioning (`ABS_X`, `ABS_Y`) and pointer buttons for spatial ray pointing.
- **Virtual Keyboard:** Full evdev keycodes with autorepeat support and automatic held key/button release on client disconnect or drop.

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

---

## 🗺️ Roadmap & Milestones

The project is structured into 7 core phases. Status reflects what is merged on `main`; see [ROADMAP.md](ROADMAP.md) for details.

- [x] **Phase 1:** Foundation & Architecture (Cargo workspace, zero-copy binary protocol, telemetry, CI)
- [~] **Phase 2:** Linux Capture & Hardware Encoding Engine (PipeWire DMA-BUF capture, NVENC AV1/HEVC with intra-refresh and reference picture invalidation done; VA-API backend for AMD/Intel implemented (HEVC, H.264 and AV1 encode, rolling intra refresh, RGB-to-NV12 via VPP) but not yet verified on hardware, see `crates/encoder/src/vaapi.rs`; select with `LINUX_QUEST_VIDEO_ENCODER=nvenc|vaapi|mock` or `linux-quest-host daemon --encoder auto|nvenc|vaapi|mock`)
- [~] **Phase 3:** Low-Latency Network & USB Transport (paced UDP, Reed-Solomon/XOR FEC, ADB reverse tunnel done; QUIC control channel and mDNS/SSDP LAN discovery pending)
- [x] **Phase 4:** Meta Quest OpenXR Client & Hardware Decoder (AMediaCodec, Vulkan, Composition Layers)
- [~] **Phase 5:** Virtual Multi-Monitor Management on Linux (wlroots, Mutter and X11 backends with dynamic EDID done; VKMS/EVDI kernel dummy-driver fallback pending)
- [x] **Phase 6:** Spatial Workspace, Passthrough & Input Redirection (6DoF screens, passthrough, controller/hand manipulation, uinput injection, clipboard sync)
- [~] **Phase 7:** Audio Streaming & Production Polish (Opus audio with A/V sync and Ratatui dashboard done; tray icon/systemd service and packaging pending)

> **Note:** Much of the code is tested against mock backends (display, decoder, uinput, clipboard, audio sink). End-to-end validation on real Quest 3 and GPU hardware, and the latency/CPU targets above, are still to be confirmed.

Detailed milestone descriptions, deliverables, and acceptance criteria are outlined in [ROADMAP.md](ROADMAP.md).

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
