# Quest hardware validation — 2026-10-21

**Outcome: NOT ACCEPTED. Refs #34; this report does not close it.** The host and
debug-signed APK built, and the APK installed without uninstalling the existing
app. The checkout's host captured a real 4K desktop with NVENC. A concurrent
hardware session then stopped that host and replaced the APK. No attributable
end-to-end benchmark of this branch completed. No optical latency was measured.

## Targets and result

Sources: [issue #34](https://github.com/Josh-Archer/linux-quest/issues/34),
[README primary goals](../README.md), [ROADMAP success metrics](../ROADMAP.md),
and [ARCHITECTURE latency budget](../ARCHITECTURE.md).

| Target | Result | Evidence / likely cause |
| --- | --- | --- |
| USB motion-to-photon / glass-to-glass <10 ms | NOT VALIDATED; USB video path unavailable | Daemon sends UDP video only; ADB reverses TCP control. See #101. No USB video FPS or latency number was obtained. |
| Wi-Fi 6 motion-to-photon / glass-to-glass <15 ms | NOT MEASURED | No optical equipment or in-headset operator confirmation available during this run. HUD timing is not optical latency; see #102. Concurrent app replacement prevented a controlled run. |
| Host CPU <5% during 3840×2160 at 90 Hz streaming | NOT VALIDATED | Actual capture negotiated 3840×2160 at **60 Hz**, Bgrx via **SHM**. The later CPU samples are an idle baseline, not streaming CPU. |
| Text: 1:1 pixel parity, no chromatic aberration or blur | NOT VALIDATED | Requires in-headset visual comparison; no operator confirmation obtained. |
| Zero crashes during resolution switches | NOT TESTED | Exclusive access unavailable; no resolution-switch cycle was performed. |
| Zero crashes during reconnects | NOT TESTED | A competing session's forced host replacement is not a controlled reconnect test. |
| Zero crashes during sleep/wake | NOT TESTED | No controlled sleep/wake cycle was performed. |
| Client video FPS and dropped frames (diagnostic; no separate numeric threshold stated) | UNAVAILABLE for this branch | Installed instrumentation was replaced before collection. Compositor FPS below does not establish fresh video delivery. |
| Host GPU (diagnostic; no numeric threshold stated) | Device utilization measured; host PID utilization unavailable | See the idle observation below. |

There is no passing performance claim in this report. Unknown measurements are
not zero and are not evidence that a target passed or numerically failed.

## Machine and build

- Host: homelabdesktop, AMD Ryzen 9 9900X, 24 logical CPUs, Ubuntu Wayland/GNOME,
  Linux 7.0.0-38-generic, NVIDIA RTX 5080, NVIDIA driver 615.71.09.
- Quest 3, USB serial `2G0YC1ZF9H039X`, authorized by ADB. Read-only Wi-Fi
  preflight reported standard 6, 6295 MHz, RSSI −46 dBm, 2401 Mbit/s link.
- Base checkout: `c8b9edf52a073638f07c01b2e89a1867d6e80d1d`, plus this PR's
  opt-in metrics export. Rust 1.98 toolchain, cargo-ndk, Android API 34,
  NDK 27.2.12479018 and SDK build-tools 35.0.0 already existed under the user's home.
- Rust was absent from PATH, not absent from the machine. No rustup installation,
  sudo, package installation, driver changes or system configuration changes were needed.
- CMake 4.2.3 rejected vendored Opus's old minimum version. Both builds succeeded
  with the process-local `CMAKE_POLICY_VERSION_MINIMUM=3.5` override.

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5 CARGO_BUILD_JOBS=8
cargo build --release --locked -p linux-quest-host --features mdns,pipewire-capture
bash crates/client/android/build.sh --release
ANDROID_SERIAL=2G0YC1ZF9H039X scripts/install-quest.sh \
  crates/client/target/android-apk/linux-quest-release.apk
```

The APK uses the CI release build/package procedure and an Android Debug signing
certificate. Initial installation rejected the newly generated debug certificate;
reusing the existing local test keystore allowed `adb install -r` to succeed.
No app uninstall or data reset was used. `apksigner verify --print-certs` confirmed
`CN=Android Debug,O=Android,C=US`.

Built artifact SHA-256:

```text
host 25e51e2e26ab30df71af2f870d20b02a1f0e287a3255be6be6a1a772c85b327d
APK  4b7fbc2cca372b16a124061d4404c3c6c80a53fd462dcb7e650d8dadf8e5d737
```

The host ran from 13:24:25Z until an external SIGINT at 13:26:19Z. At 13:24:41Z
PipeWire negotiated Bgrx 3840×2160 at 60 fps via SHM and NVENC initialized AV1,
P1 ultra-low latency, CBR 30 Mbit/s. The capture picker resolved using the stored
portal token. Startup evidence is in [checkout-host.txt](hw-validation-2026-10-21/checkout-host.txt).
This establishes capture/encoder startup, not displayed frame delivery.

The competing process in `~/lq-hwtest2` used a global host-process stop, launched
its own `--streams 3` daemon, installed its APK and relaunched the app. Hardware
mutation stopped once that conflict was identified; exclusive access was requested.
The original APK and daemon launch metadata were preserved locally in
`target/hw-validation/prior/` before the first replacement. No binaries, signing
keys, release artifacts, or full device logs are committed or published.

## Raw numbers: contended, idle observation only

To validate the collector without further changing the shared headset, a 60-sample
**read-only observation of the other session** ran at 13:29:07Z–13:30:08Z. Its
host (PID 2913794) was waiting for screen-capture permission and had not initialized
an encoder. The APK was from that other session, not this branch. These numbers
are **not streaming results** and cannot satisfy #34.

A subsequent read-back of the installed APK had SHA-256
`bbbfe7a1b02d158aa99b7f417375e69ed9ee2a1cc20645848b1bd490550219b5`,
different from this branch's built APK, confirming the replacement.

| Measurement | n | Mean | p95 | Min–max |
| --- | ---: | ---: | ---: | ---: |
| Host process CPU, 100% = one logical core | 60 | 0.164059% | 0.982941% | 0–0.999572% |
| Host process CPU divided by 24 CPUs | 60 | 0.006836% | 0.040956% | 0–0.041649% |
| Entire RTX 5080 GPU utilization | 60 | 7.45% | 10% | 6–11% |
| Entire RTX 5080 encoder utilization | 60 | 0% | 0% | 0–0% |
| Client PID 8867 VrApi compositor FPS | 60 | 90.6 | 91 | 90–91 |

- Sum of client-PID VrApi `Stale` reports: **0**. These are compositor stale-frame
  reports, not video decoder or network drops. Integer periodic FPS reports of 91
  do not mean a 90 Hz panel physically refreshed faster than 90 Hz.
- Decoder/export samples: **0**. Decoded FPS, surface-release FPS, decoder drops,
  packet loss, and HUD estimate: **unknown**, not zero.
- Per-host-PID GPU SM, memory, encode and decode utilization: **unavailable**
  (`nvidia-smi pmon` supplied no numeric counters for the idle host).
- Client PID remained 8867 across observation. This is not a reliability-cycle pass.
- Optical USB and Wi-Fi latency: **unmeasured**.

Raw evidence: [samples.jsonl](hw-validation-2026-10-21/samples.jsonl),
[summary.json](hw-validation-2026-10-21/summary.json),
[metadata.json](hw-validation-2026-10-21/metadata.json), and PID-filtered
[client-vrapi.txt](hw-validation-2026-10-21/client-vrapi.txt). Full logcat and power
dumps remain local under `target/hw-validation/`; only the relevant runtime
counter lines are committed.

## Repeatable collection

Use **exclusive access** to the host ports, desktop capture and Quest. Build and
install as above. Start the host in a terminal and select the real desktop in the
screen-sharing picker:

```bash
target/release/linux-quest-host daemon --encoder nvenc \
  --adb-serial 2G0YC1ZF9H039X >target/hw-host.log 2>&1 &
HOST_PID=$!

scripts/hw-validate.sh --serial 2G0YC1ZF9H039X --host-pid "$HOST_PID" \
  --mode usb --warmup 10 --seconds 60 --host-log target/hw-host.log \
  --output target/hw-usb

# Replace HOST_LAN_IP with the Linux machine's routable address.
scripts/hw-validate.sh --serial 2G0YC1ZF9H039X --host-pid "$HOST_PID" \
  --mode wifi --host-ip HOST_LAN_IP --warmup 10 --seconds 60 \
  --host-log target/hw-host.log --output target/hw-wifi

kill -INT "$HOST_PID"
wait "$HOST_PID"
```

The collector never replaces a host daemon or installs software. Normal collection
relaunches only linux-quest, temporarily sets `debug.linuxquest.host`, `.transport`
and `.metrics`, preserves the existing ADB reverse mapping, and restores properties
and returns Home on exit. `--observe` performs no device mutations. Output directories
must be new. Exit status 2 means no valid live video surface-release interval was
obtained; exit 0 means video counters advanced, **not that latency/CPU targets passed**.
It writes Markdown plus raw JSONL, JSON, logcat and GPU samples. Interrupted runs
write an incomplete report and attempt cleanup. Review full logs before sharing.

The opt-in `LQ_METRICS` export samples the primary stream's existing decoder,
receiver, HUD and frame-loop counters once per second. The collector checks the
host process identity, client PID, APK path and APK bytes, rejects counter resets, and
filters VrApi data to the app PID. It reports both CPU conventions because the
repo does not specify the CPU normalization. GPU device numbers include unrelated
desktop applications; PID-filtered pmon values are separately reported when supported.

`HudSnapshot.motion_to_photon_ms` is currently a refresh-rate budget plus decoder
duration plus capped frame-wait time. It omits host capture, encode and actual
transport/panel timing. `FrameLatencyBreakdown` defaults in the host dashboard are
also not measurements. No stage sum, VrApi `Lat`, predicted display time, or
surface-release timestamp is accepted as optical glass-to-glass latency.

For the acceptance rerun, use a moving desktop workload with verified negotiated
3840×2160 at 90 Hz and sustained decoded delivery; collect idle and active host
baselines. Use a calibrated high-speed camera viewing both the source display
transition and the headset display through the lens, or synchronized photodiodes,
and retain raw transitions, timing uncertainty, sample count, p50/p95/max. A
head-motion experiment is a separate motion-to-photon measurement. Repeat over
USB and Wi-Fi 6, then repeat controlled resolution, reconnect and sleep/wake cycles
and inspect text in the headset. Record any unavailable measurements explicitly.

## Follow-ups and checks

- [#101: USB video/input wiring](https://github.com/Josh-Archer/linux-quest/issues/101).
- [#102: measured latency versus HUD estimates](https://github.com/Josh-Archer/linux-quest/issues/102).
- #34 remains open for the exclusive 4K 90 Hz run, optical measurements, text
  inspection and reliability cycles.

Validation: release host and debug-signed release APK builds passed; workspace
tests **958 passed, 2 ignored**; client library tests **249 passed, 1 ignored**;
strict client Clippy, Rust formatting, ShellCheck and five collector parser tests
passed. Android compilation retains the pre-existing unused `receiver_ack` warning.

No release or deployment was created. No system configuration, headset power
override or proximity override was changed. The read-only observation left the
headset awake and the other session's app/daemon in their current state; no other
app was uninstalled, stopped or altered.
