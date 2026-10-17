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

---

## 6. Input Injection via Kernel `uinput`

To allow the Quest's physical controllers, hand tracking, or connected Bluetooth peripherals (keyboards/mice) to control the Linux desktop:
- The Quest client captures input state at high frequency (120Hz).
- Events are packed into minimal binary frames and transmitted over the low-latency socket.
- The Linux host daemon writes directly to `/dev/uinput` using `libevdev` / Rust `input-linux`, appearing as a standard physical mouse and keyboard at the kernel level without requiring window-manager-specific hacks.
