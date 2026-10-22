# Packaging and eventual store publication

Linux Quest has two products: the Ubuntu host/desktop app and the native Meta
Quest client. "App Store" here means the Meta Horizon Store. Apple distribution
would require a separate supported client and Apple build/signing pipeline.

## What the workflows produce now

- `CI`: formatting, strict Clippy, workspace tests, capture-feature checks and docs.
- `Packaging (test artifacts)`: Ubuntu desktop and daemon `.deb` packages, daemon
  `.rpm`, Arch package, Flatpak host bundle and a debug-signed Quest APK. Artifacts
  are test builds, not public store releases.
- [Home's Linux Quest packages (UECB)](https://github.com/Josh-Archer/home/actions/workflows/linux-quest-packages.yml):
  manually builds an immutable Linux Quest mainline commit on a Docker-capable
  UECB worker, inside Ubuntu 24.04. It exports desktop/daemon `.deb` packages, a daemon
  `.rpm`, `SOURCE_COMMIT` and `SHA256SUMS`. Home verifies mainline ancestry before
  allocation and finalizes the allocation after the job.
- `Stage draft release`: manually takes a successful mainline Packaging run ID
  and a version tag matching the workspace, then attaches its artifacts and
  checksums to a **draft** GitHub Release. It never uploads to either store.

The shared UECB broker currently registers runners to `Josh-Archer/home`, not
`linux-quest`. This is why its worker runs in Home. Reusing a Home runner label
in this repository would leave jobs queued for a runner registered elsewhere.
The application repository retains independent GitHub-hosted CI and test builds.
No cross-repository admin token needs to be copied into the application repository.

To trigger a UECB build:

```bash
gh workflow run linux-quest-packages.yml --repo Josh-Archer/home \
  -f source_sha=<full-mainline-commit-sha>
gh run list --repo Josh-Archer/home --workflow linux-quest-packages.yml
```

To build exactly the same Ubuntu payload locally:

```bash
docker buildx build --file packaging/ubuntu/Dockerfile --target packages \
  --build-arg SOURCE_SHA="$(git rev-parse HEAD)" \
  --output type=local,dest=artifacts .
(cd artifacts && sha256sum --check SHA256SUMS)
```

Use a clean checkout for release candidates. The container targets Ubuntu 24.04
so a newer developer desktop does not silently raise GTK or glibc requirements.
The daemon packages enable mDNS, PipeWire capture and VA-API; without real capture
support a daemon build can only provide a test pattern.

## Ubuntu

Start with the two native `.deb` files, installed together:

```bash
sudo apt install ./linux-quest-host_*.deb ./linux-quest_*.deb
```

The desktop package installs `linux-quest`, its application launcher and the logo.
The daemon package installs `linux-quest-host`, the user services and the uinput
rule. A package build does not establish real headset, portal or input behavior;
validate those on Ubuntu 24.04 with supported graphics hardware before promotion.
The install does not silently enable/start services; configure them explicitly
as described in [the packaging guide](../packaging/README.md).

For Ubuntu App Center distribution, create an Ubuntu One/Snapcraft publisher
account, register an available snap name, implement and test a Snap package,
then publish first to `edge` and promote through testing channels. The host uses
PipeWire portals, `/dev/uinput`, system services and GPU APIs, so confinement and
interface support need explicit validation. Classic confinement requires store
review; it must not be assumed approved. The existing Flatpak artifact packages
the host, not the GUI, and is not a Flathub listing.

For apt-managed updates, a signed apt repository is another option. A Launchpad
PPA specifically needs a signed **source** package and Launchpad builds; uploading
these binary `.deb` files alone is not PPA publication.

After a working Snap recipe and account are ready, add a separate manual publish
job using a channel-limited, expiring `snapcraft export-login` credential stored
as `SNAPCRAFT_STORE_CREDENTIALS` in a protected GitHub environment. Keep builds
credential-free. Do not grant PR jobs access to publisher credentials.

References:
- [Snap Store publishing](https://snapcraft.io/docs/releasing-your-app/)
- [Snapcraft authentication](https://ubuntu.com/docs/snapcraft/latest/how-to/publishing/authenticate/)
- [Classic confinement review](https://ubuntu.com/docs/snapcraft/9/explanation/classic-confinement/)
- [Launchpad source package builds](https://ubuntu.com/docs/launchpad/user/how-to/packaging/build-deb-packages-in-launchpad/)

## Meta Horizon Store

1. Create/verify the developer organization and create the Quest app in the Meta
   Horizon developer dashboard. Confirm the app ID, package name, supported
   devices and distribution agreement.
2. Establish a persistent release-signing key and back it up outside CI. The
   current Android build harness signs with a per-user debug keystore
   (`LINUX_QUEST_KEYSTORE`, default `~/.local/share/linux-quest/debug.keystore`), even with
   `--release`; that optimized APK is for sideload testing, not a store upload.
   Add an explicit store-signing mode before connecting upload credentials.
3. Use a monotonically increasing Android `versionCode`, release `versionName`,
   and the same signing identity for updates. Add the required launcher/store
   artwork and supply privacy policy, data-use disclosures, age rating and
   listing assets in the dashboard.
4. Test on actual supported headsets and satisfy Meta's current Virtual Reality
   Checks and submission requirements, including lifecycle, permissions, comfort,
   tracking, and network loss. CI compilation is not device certification.
5. Add a separate protected manual upload job using Meta's Platform CLI Utility
   (`ovr-platform-util`), an app ID and app secret/token from a GitHub environment.
   Upload signed builds to a tester release channel first, then submit the
   chosen build for store review. An upload is not store approval.

Account verification, legal agreements, store listings, privacy disclosures,
signing-key ownership and final submission remain publisher-owned steps. This
repository currently has no store credentials configured and no automatic store
upload job.

References:
- [Meta app submission](https://developers.meta.com/vr/resources/publish-submit/)
- [Release channels](https://developers.meta.com/vr/resources/publish-release-channels/)
- [Upload apps and Platform CLI](https://developers.meta.com/vr/documentation/unity/unity-platform-tool/)
