#!/usr/bin/env bash
# Sideload the linux-quest Quest client APK onto a connected Meta Quest 3 via adb.
#
# Usage: scripts/install-quest.sh [path/to/linux-quest.apk]
#
# Defaults to the APK produced by crates/client/android/build.sh
# (crates/client/target/android-apk/linux-quest-release.apk, or -debug.apk).
# Set ANDROID_SERIAL to pick one device when several are connected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
APK_DIR="${REPO_ROOT}/crates/client/target/android-apk"

die() {
    echo "ERROR: $*" >&2
    exit 1
}

if ! command -v adb >/dev/null 2>&1; then
    die "adb not found on PATH. Install Android platform-tools (e.g. 'sudo pacman -S android-tools' or 'sudo apt install adb')."
fi

APK="${1:-}"
if [[ -z "${APK}" ]]; then
    RELEASE_APK="${APK_DIR}/linux-quest-release.apk"
    DEBUG_APK="${APK_DIR}/linux-quest-debug.apk"
    # Pick the most recently built candidate (by mtime). '-nt' is portable to bash 3.2.
    if [[ -f "${RELEASE_APK}" ]]; then
        APK="${RELEASE_APK}"
    fi
    if [[ -f "${DEBUG_APK}" ]] && { [[ -z "${APK}" ]] || [[ "${DEBUG_APK}" -nt "${APK}" ]]; }; then
        APK="${DEBUG_APK}"
    fi
    if [[ -z "${APK}" ]]; then
        die "No APK given and none found in ${APK_DIR}. Run crates/client/android/build.sh first or pass a path."
    fi
    if [[ -f "${RELEASE_APK}" && -f "${DEBUG_APK}" ]]; then
        echo "WARNING: both release and debug APKs exist in ${APK_DIR}." >&2
        echo "WARNING: installing the newest one: ${APK}. Pass a path to choose explicitly." >&2
    fi
fi

if [[ ! -f "${APK}" ]]; then
    die "APK not found: ${APK}"
fi

# Make sure the adb server is up, then list devices in the "device" state.
adb start-server >/dev/null 2>&1 || die "adb could not start its server."

DEVICE_LINES="$(adb devices | tail -n +2 | grep -v '^[[:space:]]*$' || true)"
READY_SERIALS="$(printf '%s\n' "${DEVICE_LINES}" | awk -F'\t' '$2 == "device" {print $1}')"
UNAUTHORIZED="$(printf '%s\n' "${DEVICE_LINES}" | awk -F'\t' '$2 == "unauthorized" {print $1}')"

if [[ -z "${READY_SERIALS}" ]]; then
    if [[ -n "${UNAUTHORIZED}" ]]; then
        die "Device found but unauthorized. Put on the headset and accept the 'Allow USB debugging' prompt, then retry."
    fi
    die "No Quest device detected. Enable Developer Mode on the headset, connect it over USB, and accept the debugging prompt."
fi

READY_COUNT="$(printf '%s\n' "${READY_SERIALS}" | wc -l | tr -d ' ')"
ADB_ARGS=()
if [[ -n "${ANDROID_SERIAL:-}" ]]; then
    if ! printf '%s\n' "${READY_SERIALS}" | grep -qx "${ANDROID_SERIAL}"; then
        die "ANDROID_SERIAL=${ANDROID_SERIAL} is not a connected, authorized device."
    fi
    ADB_ARGS=(-s "${ANDROID_SERIAL}")
elif [[ "${READY_COUNT}" -gt 1 ]]; then
    die "Multiple devices connected. Set ANDROID_SERIAL to one of: $(printf '%s' "${READY_SERIALS}" | tr '\n' ' ')"
fi

echo "Installing ${APK} on device(s): $(printf '%s' "${READY_SERIALS}" | tr '\n' ' ')"
# ${ADB_ARGS[@]+...} expands to nothing for an empty array; a bare "${ADB_ARGS[@]}"
# trips 'unbound variable' under set -u on bash 3.2 and 4.3.
INSTALL_OUTPUT="$(adb ${ADB_ARGS[@]+"${ADB_ARGS[@]}"} install -r "${APK}" 2>&1)" || {
    echo "${INSTALL_OUTPUT}" >&2
    die "adb install failed."
}
echo "${INSTALL_OUTPUT}"

# Older adb builds can exit 0 while printing Failure; check the output too.
if ! grep -q '^Success' <<<"${INSTALL_OUTPUT}"; then
    die "adb install did not report Success."
fi

echo "Done. Launch 'linux-quest' from the Quest app library, or run:"
LAUNCH_SERIAL=""
if [[ -n "${ANDROID_SERIAL:-}" ]]; then
    LAUNCH_SERIAL="-s ${ANDROID_SERIAL} "
fi
echo "  adb ${LAUNCH_SERIAL}shell am start -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n com.linuxquest.client/android.app.NativeActivity"
