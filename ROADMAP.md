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
> Status: PipeWire/DMA-BUF capture and NVENC (incl. intra-refresh, RPI) done. VA-API backend (HEVC, H.264, rolling intra refresh, VPP colour conversion; AV1 opt-in) implemented as a scaffold, unverified on AMD/Intel hardware; the stream codec is not yet negotiated per client, and the daemon does not run a video session yet ([#28](https://github.com/Josh-Archer/linux-quest/issues/28)).

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
> Status: paced UDP, FEC, ADB bridge and the optional QUIC control transport are implemented. The host/client daemon configuration still uses TCP until certificate identity provisioning and pairing are wired in. mDNS LAN discovery is implemented, including Quest auto-connect at launch ([#30](https://github.com/Josh-Archer/linux-quest/issues/30)); SSDP is not.

*Objective: Guarantee sub-10ms delivery over wired USB and sub-15ms delivery over Wi-Fi 6/6E.*

- **3.1 USB ADB Reverse-Tethering Auto-Bridge**
  - Automatic detection of connected Meta Quest over USB.
  - Automated `adb reverse tcp:<port> tcp:<port>` setup for 0-jitter, zero packet-loss, high-bitrate (150-250 Mbps) streaming.
  - Fallback to seamless LAN discovery via mDNS (done; SSDP not implemented).

- **3.2 Custom UDP / QUIC Streaming Transport with FEC**
  - Implement packet pacing to avoid Wi-Fi burst drops.
  - Forward Error Correction (Reed-Solomon or XOR FEC) for instantaneous packet recovery without retransmission delays.
  - [x] Control, handshake, heartbeat/telemetry and clipboard messages over a reliable QUIC stream, using the existing control-message framing.
  - [x] Mutual TLS 1.3 certificate fingerprint pinning, explicit reconnect, QUIC path migration and a TCP connection-failure fallback that does not downgrade on authentication errors.
  - [x] Loopback coverage for handshake, large clipboard transfer, reconnect, migration and TCP fallback. An ignored release benchmark compares QUIC and TCP heartbeat round-trip latency; the sample result is documented in README and does not represent Wi-Fi or headset performance.
  - [ ] Pairing UX and identity persistence in host/client configuration; wire the daemon/client to select QUIC and advertise both listeners.
  - [ ] Evaluate QUIC datagrams for input/video; current video and input paths remain on their existing transports.

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
> Status: wlroots, Mutter and X11 backends done. EVDI kernel fallback implemented but not yet validated on hardware ([#31](https://github.com/Josh-Archer/linux-quest/issues/31)).

*Objective: Support multiple headless virtual monitors dynamically without physical hardware dongles.*

- **5.1 Wayland Virtual Display Management**
  - Integration with `wlr-virtual-output-v1` (Sway, Hyprland) for dynamic headless monitor provisioning.
  - Integration with GNOME / Mutter virtual display API via D-Bus (`org.gnome.Mutter.DisplayConfig`).
  - Dynamic display creation, resolution negotiation, and custom DPI configuration.

- **5.2 X11 & Headless DRM / Dummy Fallback**
  - Virtual display configuration via NVIDIA MetaModes / Xdummy on X11.
  - Kernel-level dummy driver support via the EVDI module (VKMS was dropped: its configfs connector has no EDID attribute, so a requested resolution cannot be honoured). Needs a hardware test on a machine with `evdi-dkms` loaded and no compositor virtual-output API.

---

## Phase 6: Spatial Workspace, Passthrough & Input Redirection
*Objective: Ergonomic 3D multi-screen layout, passthrough integration, and seamless keyboard/mouse control.*

- **6.1 Spatial 3D Screen Management & Passthrough**
  - [x] Enable Meta Quest Passthrough (`XR_FB_passthrough`) with graceful fallback to opaque blend mode, reconstruction layer creation beneath screens, and mock backend for testing.
  - [x] Pure-Rust 6DoF spatial screen model (`ScreenTransform`): 6DoF pose, push/pull distance, tilt, yaw, curvature mapped to Quad vs Cylinder (`XR_KHR_composition_layer_cylinder`) layers with ergonomic clamping.
  - [x] Intuitive controller and hand-tracking (`XR_EXT_hand_tracking`) window manipulation: aim pointing rays with ray-vs-quad and ray-vs-cylinder intersection, 6DoF grab/drag state machine with relative offset preservation, thumbstick push/pull distance and tilt/curve adjustment, pinch/grab gesture detection with hysteresis, and automatic layout persistence on manipulation release.

- **6.2 Kernel Input Redirection (`uinput`)**
  - [x] Remote input capture and transport (Part 1 - Issue #11):
    - Compact zero-copy binary input protocol with sequence numbers, timestamps, and redundant key transmission.
    - Quest client input capture module mapping Android `KeyEvent` / `MotionEvent` (Bluetooth keyboard/mouse and controller-as-mouse) to Linux evdev keycodes.
    - 120Hz mouse motion and scroll coalescer with sub-pixel remainder preservation.
    - Low-latency UDP input channel sender/receiver (separate from video) with stale packet dropping and host injection receiver.
  - [x] Linux `uinput` kernel device creation and physical event injection backend (Part 2 - Issue #11):
    - Virtual mouse (`REL_X`/`REL_Y`, discrete & hi-res wheels, `BTN_LEFT`/`RIGHT`/`MIDDLE`/`SIDE`/`EXTRA`), absolute pointer (`ABS_X`/`ABS_Y`), and keyboard devices.
    - `UinputDeviceFile` trait abstraction with mock writer enabling headless unit testing without `/dev/uinput`.
    - Correct `SYN_REPORT` event batching and automatic release of held keys/buttons on client disconnect or drop.
    - Clear diagnostics for missing `/dev/uinput` or permission denial with udev documentation.
  - [x] Bi-directional clipboard sharing between Quest and Linux (Part 3 - Issue #11):
    - Reliable control channel protocol messages with UTF-8 support, 4MiB size bounds, chunking for large text, and monotonic origin/version tracking.
    - Host-side `ClipboardBackend` trait with Wayland (`wl-copy`/`wl-paste`) and X11 (`xclip`/`xsel`) auto-detection and debounced change watcher.
    - Quest client `ClipboardManager` trait with Android JNI backend and headless mock.
    - Deterministic shared `ClipboardSyncEngine` with deduplication, echo loop suppression, and last-writer conflict resolution.
    - Host daemon wiring on TCP control channel alongside virtual input receivers.

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
  - [x] Lightweight terminal status dashboard (`ratatui` with crossterm backend via `dashboard` subcommand or `--tui` flag) showing:
    - Active virtual displays with resolution, refresh rate, FPS, bitrate, and codec.
    - Real-time glass-to-glass latency waterfall breakdown (Capture ➔ Encode ➔ Queue ➔ Transport ➔ Decode ➔ Render, plus audio pipeline) with budget gauges and sparklines.
    - Connected client status (USB/ADB vs Wi-Fi, smoothed RTT, packet loss, and Reed-Solomon/XOR FEC recoveries).
    - Audio stream telemetry (PipeWire capture, Opus low-delay codec, jitter, buffer delay, health, and PLC concealments).
    - Pure decoupled state/model layer with rolling window aggregation and full `TestBackend` snapshot test coverage.
  - [ ] Simple tray icon / systemd service for headless background operation ([#32](https://github.com/Josh-Archer/linux-quest/issues/32)).

- **7.3 Packaging & Distribution** ([#33](https://github.com/Josh-Archer/linux-quest/issues/33))
  - Linux packaging: `.deb`, `.rpm`, AUR (`PKGBUILD`), and Flatpak.
  - Quest client: Pre-built APK releases with fast sideloading scripts (`adb install -r`).

---

## 📈 Success Metrics
*Not yet measured on real hardware: see [#34](https://github.com/Josh-Archer/linux-quest/issues/34).*

- **Motion-to-Photon Latency:** < 10ms (USB), < 15ms (Wi-Fi 6).
- **CPU Utilization:** < 5% on host during 4K 90Hz stream.
- **Text Readability:** 1:1 pixel parity matching native monitors with no text chromatic aberration or blurring.
- **Reliability:** Zero crashing under resolution switches, reconnects, or sleep/wake cycles.
