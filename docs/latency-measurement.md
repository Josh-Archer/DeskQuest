# Latency measurement: estimates vs. glass-to-glass

Refs #34, #102. The README targets (< 10 ms USB, < 15 ms Wi-Fi) are **glass-to-glass**:
source display change to photons from the headset panel. Only an optical measurement
establishes that.

## What the software reports (all estimates)

| Figure | Where | What it is |
| --- | --- | --- |
| `Est (SW, not g2g)` | headset HUD, `hud_estimate_ms` in validation JSON | refresh-rate frame budget + render time + decoder time + `min(wait_frame, 5 ms)`. Omits host capture, encode, transport, panel scanout. |
| `stage sum` | headset HUD, host TUI waterfall | sum of the `FrameLatencyBreakdown` stages that were reported. A stage shown as `--` (0) was **not measured**; it is not a 0 ms stage. |
| `total_motion_to_photon_us` | `FrameLatencyBreakdown`, daemon dashboard | stage sum, or the headset HUD estimate. Not optical. |

Never use these, VrApi `Lat`, predicted display time, surface-release counts or
compositor FPS as a glass-to-glass result.

## Optical procedure

1. Setup: exclusive 4K 90 Hz session (verify negotiated 3840x2160 @ 90 Hz and sustained
   decoded delivery), moving desktop workload, one run each for USB and Wi-Fi.
2. Apparatus: a calibrated high-speed camera (>= 480 fps) viewing both the source display
   and the headset display through the lens, or synchronized photodiodes on both. Source
   display flips a test pattern (black to white) at known times.
3. Per sample, record the time from the source transition to the headset-panel transition.
   Collect >= 200 samples per transport. Keep the **raw samples** (CSV: `sample,
   source_ms, headset_ms, delta_ms`) alongside the report.
4. Report p50, p95 and max of `delta_ms` per transport, plus camera frame interval / photodiode
   sampling rate as the measurement uncertainty (one camera frame = +/- 1/fps).
5. Record panel scanout position (the camera must view the same screen region each time) and
   the headset and host builds.

## Follow-up (not in this change)

Per-frame stage timestamps with host/headset clock calibration and stated uncertainty
need a protocol change (frame-id-tagged stage durations plus an offset estimate) and are
tracked under #102. Re-run #34 once the apparatus and the USB video path (#101) exist.

## Measured per-frame stage timestamps (issue #102)

The HUD can now show *measured* software latency instead of the estimate:

- **Clock calibration.** The client sends a `Ping` whose payload is `LQCK` + version + `t1`.
  A new host replies with a `Pong` that appends `t2` (receive) and `t3` (send). The client
  stamps `t4` and computes NTP-style `offset = ((t2-t1)+(t3-t4))/2`, `rtt = (t4-t1)-(t3-t2)`,
  keeping the lowest-RTT sample of the last 16. Cross-clock error is bounded by `rtt/2`.
  An old host just echoes the ping; the client ignores it and keeps the estimate.
- **Frame timing.** After a host has seen a clock-sync ping it sends, per frame, a
  `TelemetryReport` with payload `LQFT` + version + `HostFrameTiming` (capture, encode start,
  encode end, last chunk sent; host clock). Old clients never receive it.
- The client records receive (last chunk), decode queued, decode dequeued and submit to the
  compositor, and combines them (`MeasuredFrameLatency`).
- HUD: `Meas cap>submit: X ms +-Y (no scanout)` plus per-stage `Enc/Snd/Net/Buf/Dec/Pres`
  when calibrated; otherwise `Est (SW, not g2g)` stays, with any single-clock stages shown as
  `Meas stages (no clock sync)`. Measured figures still stop at compositor submit: panel
  scanout and pixel response are only covered by the optical procedure above.

Wire format and math: `crates/protocol/src/timing.rs`.

### Daemon path

`HostStreamSession` answers on its own transport. The daemon splits the job:

- The client sends the `LQCK` ping as a serialized `Packet` on the input channel: the UDP
  input port (only from the latched peer, or anyone before one is latched) or the USB tunnel.
  The input receiver stamps `t2` on arrival and queues it in `ClockSyncRelay`
  (`crates/host/src/clock_sync.rs`, shared through `KeyframeRequests`).
- The stream 0 video sender answers before its next frame with a `Pong` on the video path,
  stamping `t3` just before sending, so time queued in the relay is host processing, not network.
- Once a client has pinged, stream 0 sends one `LQFT` report per frame it fully sent.
  `capture_us` is when the capture task handed the frame to the sender (the compositor-to-capture
  delay is not covered). Encode start/end bracket the encoder call; `send_us` follows the last
  datagram.
- `Pong` and `LQFT` use sequence 0 so they stay outside the video sequence space. The client
  takes them before its jitter buffer.

Still pending: the Android client does not send clock-sync pings yet (`set_clock_sync_sender`
is not wired to its UDP input socket or USB tunnel), so the HUD keeps the estimate there.
