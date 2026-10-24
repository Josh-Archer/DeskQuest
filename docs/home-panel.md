# Quest Home panel and hand scrolling

DeskQuest opens as a resizable desktop panel in Horizon Home on Horizon OS 69 or later.
The panel displays the primary monitor (stream 0). Choose **Open VR** for the immersive
workspace, including its spatial monitor placement and additional monitors.

Platform setup follows [Meta's hybrid app guide](https://developers.meta.com/vr/documentation/spatial-sdk/hybrid-apps-overview/).

The panel uses the existing receiver, FEC, keyframe recovery and hardware decoder. It
decodes the host's AV1 or HEVC stream at its transmitted resolution and preserves its
aspect ratio when the panel is resized. The host selection and `debug.linuxquest.*`
launch overrides are shared with the immersive client.

## Controls in Home

Horizon sends hand and controller interactions with the panel as Android touch events.

- Tap or pinch briefly to click.
- Pinch and move over the desktop to scroll. Content follows the movement.
- Hold still until the long press activates, then move to drag with the left mouse button.
- A Bluetooth mouse supports pointer movement, buttons and wheel scrolling.
- **Keyboard** opens the Horizon keyboard. A physical keyboard also works when the desktop has focus.
- **Reconnect** restarts discovery and the stream.
- **Open VR** releases the panel's decoder and transport before launching the immersive activity.

Touch, keyboard and gesture input use the existing Wi-Fi input channel or the negotiated
USB tunnel. USB video and input share the tunnel's reconnect and heartbeat handling.

## Controls in immersive VR

Aim at a desktop panel, pinch **thumb and middle finger** together with the index finger
released, then move the hand up, down or sideways to scroll. Content follows the hand.
Release the middle pinch to stop. The cursor stays anchored on the panel during the
gesture. Index pinch continues to click or drag, and the controller thumbstick scrolls.

Scrolling requires valid tracked thumb, middle and index tips. Tracking loss, changing
input source or removing the target screen ends the gesture. A small movement dead
zone avoids scrolling from pinch jitter, and tracking jumps do not produce scroll bursts.

## Build and launch

```sh
crates/client/android/build.sh --install --run
# Or launch immersive VR directly:
crates/client/android/build.sh --install --run --vr
```

Explicit activity names for ADB:

```sh
adb shell am start -n com.linuxquest.client/.HomePanelActivity
adb shell am start -n com.linuxquest.client/.VrActivity
```

The Home panel and immersive activity use separate processes and exchange private mode
switch broadcasts to close the previous presentation. A visible panel keeps streaming
when another panel takes focus. Focus loss releases held mouse buttons and keyboard keys;
surface destruction or activity stop also closes the panel's worker.
