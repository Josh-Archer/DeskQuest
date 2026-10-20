# VA-API manual hardware validation

The VA-API backend is exercised in CI through `MockVaLayer`; these steps check the real
driver, DMA-BUF import, bitstream and headset decoder. Run them on both an AMD and an
Intel GPU before marking hardware support verified.

## Prepare the host

1. Install the distribution's VA-API runtime, `vainfo`, and the correct vendor driver
   (Mesa VA drivers for AMD, Intel media driver for supported Intel GPUs). Ensure the
   user running the host can open the GPU's `/dev/dri/renderD*` node.
2. Confirm the selected driver is active with `vainfo`. Record the reported vendor,
   supported encode profiles and entrypoints. Confirm the target codec advertises CBR.
3. Build with the VA-API feature and the capture backend used by the host:

   ```bash
   cargo build -p linux-quest-host --features vaapi,pipewire-capture
   ```

## Exercise the backend

1. Start the host with VA-API explicitly selected so that NVENC cannot mask a failure:

   ```bash
   RUST_LOG=info cargo run -p linux-quest-host --features vaapi,pipewire-capture -- \
     daemon --encoder vaapi
   ```

2. Confirm logs name the VA vendor, selected profile and entrypoint. Check that the
   first encoded frame is an IDR and that following frames arrive without a periodic
   full-frame burst when rolling intra-refresh is advertised.
3. Stream to a Quest client and verify decoded video, resolution, colour and sustained
   frame delivery. Repeat with HEVC and H.264. Test AV1 separately only when the
   driver advertises AV1 encode and the experiment is explicitly enabled:

   ```bash
   LINUX_QUEST_VAAPI_AV1=1 RUST_LOG=info cargo run -p linux-quest-host \
     --features vaapi,pipewire-capture -- daemon --encoder vaapi
   ```

4. Confirm the capture source delivers DMA-BUF frames. NV12 should use direct PRIME
   import. RGB frames need the driver's VA video-processing entrypoint for zero-copy
   conversion. Logs identify CPU fallback when import or VPP is unavailable; record
   that as a failed zero-copy check rather than a successful DMA-BUF validation.
5. Trigger a keyframe request and a reference-picture invalidation through the
   streaming feedback path, then confirm the stream recovers and continues decoding.
6. Repeat the same checks on the other vendor, record GPU model, driver and libva
   versions, codec, resolution, frame rate, observed import path and decode result.

If probing fails, preserve the `vainfo` output and backend error reason. A driver that
does not advertise CBR or the requested codec/profile is reported as unavailable and
is skipped by automatic selection.
