# Headset models

Original approximate geometry authored for Linux Quest, under this repository’s license. The Quest 3 shell has a continuous rounded profile and three pill-shaped sensor housings, with straps, facial cushion and side arms. This is an app illustration, not manufacturer CAD. Visual reference: [Meta Quest 3](https://www.meta.com/quest/quest-3/).

Editable source: `art/blender/quest-3.blend`. The procedural recipe is `art/blender/create_headset.py`:

```bash
blender --background --python art/blender/create_headset.py -- --root "$PWD"
```

`quest-3.glb` is the portable 3D export. `quest-3.lqmesh` and `generic.lqmesh` are the application’s embedded geometry; Blender is not required at runtime. LQM2 stores a magic header, little-endian triangle count and 21 little-endian float32 values per triangle: three positions, three split normals and RGB material color. Coordinates use metres, X right, Y up and Z forward. The native renderer interpolates lighting and uses a depth buffer, with no continuous animation loop. The small layout marker is cached per model and display scale.

The connected headset app’s reported model takes precedence. When it is unavailable, a read-only `adb devices -l` query detects a single authorized Quest on USB. Queries are bounded to one second and cached for five seconds; no USB queries are made while the app reports a connected headset. No pairing, tunnel, package install or device-setting changes are performed. USB presence is labelled separately from an app connection. Multiple devices remain unselected; unauthorized/offline devices and wireless ADB are ignored. Quest 3S and unrecognized model names use the clearly labelled generic model.

The model at the centre of the screen layout marks its fixed origin. It does not display live head pose. Click the device’s “3D” button to rotate and zoom its larger preview.

The PNG is a transparent Blender authoring preview, not a headset capture. Native rendering and detection have separate tests. A local optimized 640×420 rendering check averaged about 4.5 ms per image; timing depends on the machine and viewport size.
