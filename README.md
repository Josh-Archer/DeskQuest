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
| **Input Redirection** | **Linux `uinput`** | Kernel-level low-overhead virtual keyboard and mouse emulation. |

---

## 🗺️ Roadmap & Milestones

The project is structured into 7 core phases:

- [ ] **Phase 1:** Foundation & Architecture (Monorepo scaffolding, binary protocol, telemetry)
- [ ] **Phase 2:** Linux Capture & Hardware Encoding Engine (PipeWire DMA-BUF, NVENC/VA-API AV1/HEVC)
- [ ] **Phase 3:** Low-Latency Network & USB Transport (QUIC/UDP, FEC, ADB reverse tunnel)
- [ ] **Phase 4:** Meta Quest OpenXR Client & Hardware Decoder (AMediaCodec, Vulkan, Composition Layers)
- [ ] **Phase 5:** Virtual Multi-Monitor Management on Linux (Wayland virtual outputs, DRM/KMS, dynamic EDID)
- [ ] **Phase 6:** Spatial Workspace, Passthrough & Input Redirection (6DoF pose, passthrough layers, controller/hand 6DoF manipulation & layout persistence completed; host input injection in progress)
- [ ] **Phase 7:** Audio Streaming & Production Polish (PipeWire Opus, packaging, CLI/TUI)

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
