# Project Roadmap: linux-quest

This roadmap outlines the development trajectory of `linux-quest` from initial architectural scaffolding to a fully featured, production-ready spatial desktop system.

---

## 🎯 High-Level Timeline & Phases

```
+--------------------------------------------------------------------------+
| Phase 1: Foundation, Workspace Scaffolding & Binary Protocol             |
+--------------------------------------------------------------------------+
                                     │
                                     ▼
+--------------------------------------------------------------------------+
| Phase 2: Linux Capture & Hardware Encoding Engine (NVENC / VA-API)       |
+--------------------------------------------------------------------------+
                                     │
                                     ▼
+--------------------------------------------------------------------------+
| Phase 3: Ultra-Low-Latency Network Transport & USB ADB Tethering         |
+--------------------------------------------------------------------------+
                                     │
                                     ▼
+--------------------------------------------------------------------------+
| Phase 4: Quest OpenXR Client & Hardware Decoder (AMediaCodec + Layers)   |
+--------------------------------------------------------------------------+
                                     │
                                     ▼
+--------------------------------------------------------------------------+
| Phase 5: Virtual Multi-Monitor Management on Linux (Wayland / DRM)       |
+--------------------------------------------------------------------------+
                                     │
                                     ▼
+--------------------------------------------------------------------------+
| Phase 6: Spatial Workspace, Passthrough & Input Redirection (uinput)     |
+--------------------------------------------------------------------------+
                                     │
                                     ▼
+--------------------------------------------------------------------------+
| Phase 7: Audio Streaming, UI/TUI Management & Production Polish          |
+--------------------------------------------------------------------------+
```

---

## Phase 1: Foundation, Workspace Scaffolding & Binary Protocol
*Objective: Establish the Rust workspace, shared type definitions, telemetry, and low-overhead binary framing protocol.*

- **1.1 Rust Workspace Setup & Scaffolding**
  - Initialize Cargo workspace structure:
    - `crates/linux-quest-protocol`: Shared serialization, control packets, video frame headers, input events.
    - `crates/linux-quest-host`: Daemon entry point for the Linux desktop streamer.
    - `crates/linux-quest-capture`: Screen capture abstractions (PipeWire, DMA-BUF, X11/NVFBC).
    - `crates/linux-quest-encoder`: Hardware video encoding engine (NVENC, VA-API).
    - `crates/linux-quest-transport`: Network and USB transport abstractions (QUIC, raw UDP, ADB bridge).
    - `crates/linux-quest-input`: Linux kernel input injection via `uinput`.
    - `client/quest-client`: Android / OpenXR Rust/Vulkan client for Meta Quest.
  - Setup CI workflow (GitHub Actions) for `cargo check`, `cargo clippy`, and unit testing.

- **1.2 Zero-Overhead Binary Protocol Definition**
  - Implement zero-copy framing protocol using `postcard` or `bincode` for control and telemetry messages.
  - Video stream packetization format: NALU/OBU chunking, RTP/custom sequence headers, timestamp synchronization, and frame markers.
  - End-to-end latency telemetry specification (Capture timestamp -> Encode start/end -> Transmit -> Receive -> Decode -> VSync presentation).

---

## Phase 2: Linux Capture & Hardware Encoding Engine
*Objective: Build a zero-copy capture pipeline on Linux capable of 4K @ 90Hz/120Hz with minimal CPU overhead.*

- **2.1 PipeWire & DMA-BUF Zero-Copy Capture**
  - Integrate with `libpipewire` (via `pipewire-rs`) and Wayland screencast portals (`xdg-desktop-portal`).
  - Implement DMA-BUF file descriptor export to preserve GPU memory without round-trips to host RAM.
  - Support fallback capture paths: X11 XSHM / XDamage and direct DRM/KMS dumb buffers.

- **2.2 NVENC Hardware Encoding Integration**
  - Direct NVENC API bindings in Rust (CBR, ultra-low latency P1/P2 preset, zero B-frames).
  - Codec support: AV1 (primary for Quest 3 / RTX 40 & 50 series) and HEVC/H.265 fallback.
  - Implement intra-refresh / slice-based encoding to avoid full I-frame packet bursts and eliminate stutter when packets drop.
  - Reference Picture Invalidation (RPI) feedback loop from client to host.

- **2.3 VA-API / Intel & AMD Encoding Integration**
  - Provide a modular backend for AMD and Intel GPUs via VA-API / Vulkan Video Encode.

---

## Phase 3: Ultra-Low-Latency Network & USB Transport
*Objective: Guarantee sub-10ms delivery over wired USB and sub-15ms delivery over Wi-Fi 6/6E.*

- **3.1 USB ADB Reverse-Tethering Auto-Bridge**
  - Automatic detection of connected Meta Quest over USB.
  - Automated `adb reverse tcp:<port> tcp:<port>` setup for 0-jitter, zero packet-loss, high-bitrate (150-250 Mbps) streaming.
  - Fallback to seamless LAN discovery via mDNS / SSDP.

- **3.2 Custom UDP / QUIC Streaming Transport with FEC**
  - Implement packet pacing to avoid Wi-Fi burst drops.
  - Forward Error Correction (Reed-Solomon or XOR FEC) for instantaneous packet recovery without retransmission delays.
  - Control channel over QUIC streams or reliable WebSocket.

---

## Phase 4: Meta Quest OpenXR Client & Hardware Decoder
*Objective: Native Quest 3 client rendering with sub-pixel text clarity.*

- **4.1 OpenXR + Vulkan Android Runtime Scaffold**
  - Build Quest 3 client using Rust (`openxr` + `ash`) on `android-activity` or native Android NDK harness.
  - Initialize OpenXR session on Horizon OS with Vulkan graphics binding.
  - Frame pacing synchronized to display refresh rate (72Hz / 90Hz / 120Hz).

- **4.2 AMediaCodec Zero-Copy Hardware Decoding**
  - Asynchronous `AMediaCodec` pipeline decoding AV1 / HEVC directly into Vulkan external textures (`AHardwareBuffer` / `SurfaceTexture`).
  - Zero memory copies between Android decoder and OpenXR composition buffers.

- **4.3 OpenXR Composition Layers for Text Crispness**
  - Direct blitting to OpenXR Composition Layers (`XR_KHR_composition_layer_cylinder` and `XR_KHR_composition_layer_equirect2`).
  - Bypass intermediate 3D scene rendering passes to leverage Quest's native timewarp compositor for razor-sharp IDE code and terminal text.

---

## Phase 5: Virtual Multi-Monitor Management on Linux
*Objective: Support multiple headless virtual monitors dynamically without physical hardware dongles.*

- **5.1 Wayland Virtual Display Management**
  - Integration with `wlr-virtual-output-v1` (Sway, Hyprland) for dynamic headless monitor provisioning.
  - Integration with GNOME / Mutter virtual display API via D-Bus (`org.gnome.Mutter.DisplayConfig`).
  - Dynamic display creation, resolution negotiation, and custom DPI configuration.

- **5.2 X11 & Headless DRM / Dummy Fallback**
  - Virtual display configuration via NVIDIA MetaModes / Xdummy on X11.
  - Kernel-level dummy driver support (VKMS or EVDI module integration).

---

## Phase 6: Spatial Workspace, Passthrough & Input Redirection
*Objective: Ergonomic 3D multi-screen layout, passthrough integration, and seamless keyboard/mouse control.*

- **6.1 Spatial 3D Screen Management & Passthrough**
  - Enable Meta Quest Passthrough (`XR_FB_passthrough`) to keep physical keyboard and desk visible.
  - 6DoF virtual monitor positioning: curved angles, distance, height, tilt, and screen scale.
  - Intuitive controller and hand-tracking (`XR_EXT_hand_tracking`) window manipulation.

- **6.2 Kernel Input Redirection (`uinput`)**
  - Intercept Quest mouse / trackpad / keyboard events (or Bluetooth devices connected to the headset).
  - Forward low-latency input packets to Linux host and inject via Linux `uinput` kernel subsystem.
  - Bi-directional clipboard sharing between Quest and Linux (X11 / Wayland clipboard protocols).

---

## Phase 7: Audio Streaming, UI/TUI & Production Polish
*Objective: Complete multimedia experience, intuitive host controls, and frictionless installation.*

- **7.1 PipeWire Audio Streaming (Opus)**
  - [x] Host audio capture (`AudioSource` trait, PipeWire sink-monitor backend behind `pipewire-audio` feature, 48kHz stereo f32/i16, synthetic sine generator).
  - [x] Low-latency Opus encoding (`AudioEncoder` trait, 2.5ms/5ms frames, restricted low-delay application, configurable bitrate, FEC/DTX).
  - [x] Binary protocol audio packetization (`AudioPacket`, `PacketType::AudioFrame`) with capture timestamps synchronized to video presentation clock.
  - [x] Host streaming pipeline (`HostAudioPipeline`) transmitting audio packets over TCP, UDP, and loopback transports.
  - [x] Synchronized audio playback and jitter buffer integration on Quest client (`AudioReceiver` with adaptive jitter buffer keyed by sequence number, Opus decoding with PLC/FEC behind `AudioDecoder` trait, A/V presentation clock synchronization with drift correction by small resampling or drop/insert, and `AudioSink` trait with Android AAudio low-latency mode and mock sink).

- **7.2 Host Management CLI & TUI**
  - Lightweight terminal UI (using `ratatui`) showing real-time latency stats (capture, encode, network, decode), connected headsets, and virtual display toggles.
  - Simple tray icon / systemd service for headless background operation.

- **7.3 Packaging & Distribution**
  - Linux packaging: `.deb`, `.rpm`, AUR (`PKGBUILD`), and Flatpak.
  - Quest client: Pre-built APK releases with fast sideloading scripts (`adb install -r`).

---

## 📈 Success Metrics
- **Motion-to-Photon Latency:** < 10ms (USB), < 15ms (Wi-Fi 6).
- **CPU Utilization:** < 5% on host during 4K 90Hz stream.
- **Text Readability:** 1:1 pixel parity matching native monitors with no text chromatic aberration or blurring.
- **Reliability:** Zero crashing under resolution switches, reconnects, or sleep/wake cycles.
