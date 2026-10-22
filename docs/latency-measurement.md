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
