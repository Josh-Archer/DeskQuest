# Technical Architecture: linux-quest

This document details the systems architecture, data pipeline, and zero-copy memory flow of `linux-quest`.

---

## 1. End-to-End Latency Budget

To deliver a native-feeling desktop experience, our target end-to-end latency budget is **under 10ms over USB** and **under 15ms over Wi-Fi 6**:

```
+----------------------------------------------------------------------------------------+
| Pipeline Stage      | Target Duration | Implementation Strategy                        |
+---------------------+-----------------+------------------------------------------------+
| 1. Capture          | 0.5 - 1.5 ms    | DMA-BUF / PipeWire zero-copy GPU handle        |
| 2. Hardware Encode  | 2.0 - 3.5 ms    | NVENC / VA-API (AV1, P1 ultra-low latency, RPI)|
| 3. Packetize & Send | 0.2 - 0.5 ms    | Zero-copy slicing, paced UDP datagrams / ADB   |
| 4. Network Transit  | 0.5 - 2.5 ms    | USB 3.0 (<0.5ms) or Wi-Fi 6 5/6GHz (1-3ms)     |
| 5. Hardware Decode  | 2.5 - 4.5 ms    | AMediaCodec direct to Vulkan AHardwareBuffer   |
| 6. Composition Blit | 0.5 - 1.0 ms    | OpenXR Hardware Composition Layer sampling     |
+---------------------+-----------------+------------------------------------------------+
| TOTAL LATENCY       | 6.2 - 13.5 ms   | MOTION-TO-PHOTON GLASS-TO-GLASS                |
+----------------------------------------------------------------------------------------+
```

---

## 2. Zero-Copy Host Capture Pipeline

Traditional screen-recording tools read pixel data back to CPU system memory (`glReadPixels` or CPU SHM), copy it into an encoder buffer, and feed it to the GPU encoder. This creates major memory bandwidth bottlenecks and adds 10-30ms of latency.

`linux-quest` uses a strictly zero-copy GPU pipeline:

```
[Compositor Framebuffer (VRAM)]
              │
              ▼ (DMA-BUF File Descriptor export via PipeWire / EGL)
[GPU Memory Texture Handle]
              │
              ▼ (Zero-copy CUDA / VA-API Interop via EGLImage or NvEglStream)
[NVENC / VA-API Encoder Core]
              │
              ▼ (Bitstream Packet directly into ring-buffer)
[Network Socket / ADB Pipe]
```

### Video Codec Selection
1. **AV1 (Primary):**
   - Supported natively by modern host GPUs (NVIDIA RTX 40/50 series, Intel Arc, AMD RDNA3) and decoded in hardware by the Meta Quest 3's Snapdragon XR2 Gen 2 processor.
   - Provides superior compression efficiency at high resolutions (4K per eye), preventing color fringing on high-contrast text and fine UI lines.
2. **HEVC / H.265 (Fallback):**
   - High-performance, widely supported fallback for RTX 20/30 series and older platforms.

### Rate Control & Anti-Stutter
- **Intra-Refresh (Slice-based encoding):** Instead of sending heavy periodic IDR (I-frames) that saturate the network and cause frame drops, intra-refresh encodes a vertical column of macroblocks as intra-coded across successive P-frames.
- **Reference Picture Invalidation (RPI):** When a network packet loss occurs, the client sends an RPI feedback signal with the last acknowledged frame ID. The host encoder immediately references that frame instead of dropping frames or requiring an IDR reset.

---

## 3. High-Fidelity Text Rendering via OpenXR Composition Layers

A major flaw in previous VR virtual desktop apps is rendering desktop planes as standard 3D meshes inside a 3D scene engine. This leads to:
1. Resampling artifacts (downsampling/upsampling across multiple render targets).
2. Blurry text caused by perspective distortion and anti-aliasing passes.
3. Extra GPU overhead on the Quest headset.

### The Solution: Direct Composition Layers
`linux-quest` bypasses the standard 3D world render pass for desktop displays:
- Streams are decoded directly into swapchains bound to **`XrCompositionLayerQuad`** or **`XrCompositionLayerCylinderKHR`**.
- The Meta Horizon OS system compositor samples the video texture directly during its final Timewarp pass, yielding 1:1 crispness, sub-pixel text filtering, and chromatic aberration correction at native display resolution.

---

## 4. Virtual Multi-Monitor Management on Linux

To support multi-monitor productivity without requiring physical HDMI/DisplayPort dummy plugs, `linux-quest` manages headless virtual outputs:

### Wayland Support:
- **`wlr-virtual-output-v1`**: Supported by Sway, Hyprland, and other wlroots compositors. Creates arbitrary virtual displays with custom resolutions and refresh rates.
- **Mutter / GNOME D-Bus**: Interfaces with GNOME's `org.gnome.Mutter.DisplayConfig` to spawn virtual monitors on standard Ubuntu GNOME desktop sessions.

### X11 Support:
- Dynamic virtual monitors using NVIDIA MetaModes or `xrandr` virtual outputs.
- Integration with the out-of-tree EVDI kernel module as a last-resort fallback (`crates/display/src/backend/kernel_drm.rs`): the backend reuses an idle EVDI card or adds one, connects a generated EDID, and keeps the card fd open (and serviced by a small thread) for as long as the monitor should exist; closing the fd disconnects it. VKMS is not supported because its configfs connector cannot take an EDID.

---

## 5. Transport: Dual-Path USB and Wi-Fi Streaming

### USB Tethering (ADB Bridge)
For the lowest possible latency and maximum stability during extended desk-bound work sessions:
- Client connects via USB-C.
- Host daemon automatically registers an `adb reverse` TCP tunnel.
- Bypasses Wi-Fi radio congestion, power-saving throttles, and channel interference entirely, sustaining bitrates up to 250 Mbps with < 0.5ms network jitter.

### Wi-Fi Transport (QUIC / Custom UDP)
For wireless freedom:
- Paced UDP datagram streaming with Reed-Solomon Forward Error Correction (FEC).
- Jitter buffer dynamically tuned based on round-trip time (RTT) moving averages.
- Out-of-band reliable control channel for handshake, screen geometry, resolution updates, and encryption keys.

### Ports
| Port | Proto | Purpose |
|------|-------|---------|
| 48444 | TCP | Host control channel (clipboard, signaling). Also the `adb reverse` port: the Quest connects to `127.0.0.1:48444` over USB. |
| 48442 | UDP | Low-latency input channel (Quest to host). In `Udp` mode the client sends input and a 100 ms keepalive here from an ephemeral local port; the host latches that peer and streams video to its IP on 48440. `adb reverse` cannot carry UDP, so the USB mode sends no input yet. |

Status: the host has no video endpoint yet. The daemon (`linux-quest-host daemon`, headless and
`--tui`) starts the ADB bridge (`crates/host/src/adb_link.rs`) and keeps the reverse tunnel in place.

- **USB tunnel management.** While a Quest is attached and authorized, the daemon runs
  `adb reverse tcp:48444 tcp:48444` and re-applies it after a detach, flap or dropped rule. Tunnel
  up/down events feed `LinkTracker::adb_event`, so a loopback control connection is reported as USB
  only while the tunnel is up (loopback without a tunnel is reported as disconnected).
- **When it cannot connect** the daemon never fails. It logs one message per change of situation
  (not per poll) and keeps polling (every 2 s; every 15 s when `adb` itself is missing or failing):
  `adb` not on PATH, adb server errors, no device, a device that is not a Quest, an unauthorized
  device (accept the USB debugging prompt in the headset), or several Quests attached (it will not
  guess; pass `--adb-serial <serial>`).
- **Flags.** `--no-adb` disables the bridge (the daemon then never runs `adb`; a manually
  created `adb reverse` still carries traffic, but nothing reports the tunnel as up, so loopback
  connections show as disconnected).
  `--adb-serial <serial>` picks one device and skips the Quest model check. There is no env var.
- **Shutdown** (SIGINT/SIGTERM, or leaving the TUI) removes the reverse mapping on a best-effort
  basis (3 s limit) and marks the tunnel down. After a crash the mapping can survive in the adb
  server; the next start re-applies it, replacing the old one.

#### What works over USB today

`adb reverse` forwards **TCP only**; it cannot carry UDP. So over the USB tunnel:

| Channel | Port | Over USB tunnel | Notes |
|---------|------|-----------------|-------|
| Control (clipboard, link detection) | TCP 48444 | Yes | The daemon's control listener accepts the tunneled connection and classifies it as USB. |
| Input | UDP 48442 | **No** | The host's UDP input receiver never sees tunneled traffic. A client would have to reach it over the network (Wi-Fi, or USB tethering/RNDIS addressed by IP), not `127.0.0.1`. |
| Video | none | **No** | The host has no video endpoint. The Android client's USB mode (`TransportMode::AdbReverseTcp`) connects to `127.0.0.1:48444` and only reads video `Packet`s from that TCP stream, but the control listener speaks `ControlMessage` frames, not video packets. |

The Android client does not send input or control messages at all today (`crates/client` uses neither
`InputChannel` nor `ControlChannel`), so over USB today the host can show "connected via USB" for a
tunneled control connection, but no input and no video actually flow.
Making USB useful needs, at minimum, input carried over TCP (for example `InputEvent` frames on the
control connection, or a second reversed TCP port) and a TCP video endpoint on the host; both are
protocol changes that are out of scope here.

- Wi-Fi/LAN (manual entry and mDNS discovery): the host's control listener, TCP `48444`. Discovery advertises this port, along with the UDP input port `48442`. Manual entry defaults to `48444`.
- USB/ADB: `adb reverse tcp:48444 tcp:48444`, the same control port, so the USB host address is `127.0.0.1:48444`.
- Wi-Fi video: UDP `48440` on the client. The client accepts datagrams only from the host IP it was given.
  - In `Udp` mode the default `host_addr` is `127.0.0.1`, so **all LAN video is dropped until discovery or manual entry sets the host address**. The client logs a warning at startup in that state, and a rate-limited warning for the first dropped datagram from each source. `0.0.0.0` disables the filter.
  - Discovery picks the host's LAN address (private ranges ahead of the Docker bridge `172.17/16`) as `host_addr` and keeps the host's other discovered addresses, so the filter accepts video from any of them. Addresses are compared in canonical form (IPv4-mapped IPv6 equals IPv4).
- Discovery duty cycle: while disconnected the client browses for 2 s every 15 s, holding the Android multicast lock for about 13% of the time. Hosts stay listed for 45 s after they last answered.
- The daemon has no video listener yet, so no video port is advertised.

---

## 6. Input Injection via Kernel `uinput`

To allow the Quest's physical controllers, hand tracking, or connected Bluetooth peripherals (keyboards/mice) to control the Linux desktop:
- The Quest client captures input state at high frequency (120Hz).
- Events are packed into minimal binary frames and transmitted over the low-latency socket.
- The Linux host daemon writes directly to `/dev/uinput` using `libevdev` / Rust `input-linux`, appearing as a standard physical mouse and keyboard at the kernel level without requiring window-manager-specific hacks.
