#!/usr/bin/env bash
set -euo pipefail

# linux-quest Meta Quest 3 Android APK build and package harness
# Targets 64-bit ARM (arm64-v8a / aarch64-linux-android) on Meta Horizon OS.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLIENT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
WORKSPACE_ROOT="$(cd "${CLIENT_DIR}/../.." && pwd)"

# Detect Android SDK and NDK
ANDROID_SDK_ROOT="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-/home/gawly/Android/Sdk}}"
if [[ ! -d "${ANDROID_SDK_ROOT}" ]]; then
    echo "ERROR: Android SDK not found at ${ANDROID_SDK_ROOT}. Set ANDROID_SDK_ROOT." >&2
    exit 1
fi

ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-${ANDROID_SDK_ROOT}/ndk/27.2.12479018}"
if [[ ! -d "${ANDROID_NDK_HOME}" ]]; then
    # Fallback to any installed NDK
    ANDROID_NDK_HOME="$(find "${ANDROID_SDK_ROOT}/ndk" -maxdepth 1 -mindepth 1 | sort -V | tail -n1 || true)"
fi

if [[ -z "${ANDROID_NDK_HOME}" || ! -d "${ANDROID_NDK_HOME}" ]]; then
    echo "ERROR: Android NDK not found. Set ANDROID_NDK_HOME." >&2
    exit 1
fi
export ANDROID_NDK_HOME

# Locate build tools
BUILD_TOOLS_DIR="$(find "${ANDROID_SDK_ROOT}/build-tools" -maxdepth 1 -mindepth 1 | sort -V | tail -n1)"
if [[ -z "${BUILD_TOOLS_DIR}" || ! -d "${BUILD_TOOLS_DIR}" ]]; then
    echo "ERROR: Android build-tools not found in ${ANDROID_SDK_ROOT}/build-tools" >&2
    exit 1
fi

AAPT="${BUILD_TOOLS_DIR}/aapt"
D8="${BUILD_TOOLS_DIR}/d8"
ZIPALIGN="${BUILD_TOOLS_DIR}/zipalign"
APKSIGNER="${BUILD_TOOLS_DIR}/apksigner"
ANDROID_JAR="${ANDROID_SDK_ROOT}/platforms/android-34/android.jar"
ADB="${ANDROID_SDK_ROOT}/platform-tools/adb"
if command -v adb &> /dev/null; then
    ADB="$(command -v adb)"
fi

if [[ ! -f "${ANDROID_JAR}" ]]; then
    ANDROID_JAR="$(find "${ANDROID_SDK_ROOT}/platforms" -name "android.jar" | sort -V | tail -n1 || true)"
fi

if [[ ! -f "${D8}" ]]; then
    echo "ERROR: d8 not found in ${BUILD_TOOLS_DIR}." >&2
    exit 1
fi

if [[ ! -f "${ANDROID_JAR}" ]]; then
    echo "ERROR: android.jar not found under ${ANDROID_SDK_ROOT}/platforms" >&2
    exit 1
fi

# Parse arguments
BUILD_MODE="debug"
CARGO_PROFILE_FLAG=""
TARGET_DIR="${WORKSPACE_ROOT}/target/aarch64-linux-android/debug"
INSTALL_ON_DEVICE=false
RUN_ON_DEVICE=false

for arg in "$@"; do
    case "$arg" in
        --release)
            BUILD_MODE="release"
            CARGO_PROFILE_FLAG="--release"
            TARGET_DIR="${WORKSPACE_ROOT}/target/aarch64-linux-android/release"
            ;;
        --install)
            INSTALL_ON_DEVICE=true
            ;;
        --run)
            INSTALL_ON_DEVICE=true
            RUN_ON_DEVICE=true
            ;;
        -h|--help)
            echo "Usage: $0 [--release] [--install] [--run]"
            echo "  --release  Build optimized release APK"
            echo "  --install  Install APK on connected Meta Quest via adb"
            echo "  --run      Install and launch APK on connected Meta Quest"
            exit 0
            ;;
    esac
done

echo "========================================================"
echo "linux-quest Quest 3 Client Packaging Harness"
echo "Build Mode:       ${BUILD_MODE}"
echo "Android SDK:      ${ANDROID_SDK_ROOT}"
echo "Android NDK:      ${ANDROID_NDK_HOME}"
echo "Build Tools:      ${BUILD_TOOLS_DIR}"
echo "Android Platform: ${ANDROID_JAR}"
echo "========================================================"

# cmake-built dependencies (OpenXR loader, Opus) for this target use the NDK toolchain wrapper.
export CMAKE_TOOLCHAIN_FILE_aarch64_linux_android="${CMAKE_TOOLCHAIN_FILE_aarch64_linux_android:-${SCRIPT_DIR}/ndk-cmake-toolchain.cmake}"

# Step 1: Compile native shared library with cargo-ndk
echo "[1/6] Compiling native cdylib with cargo-ndk (arm64-v8a, API 30)..."
cargo ndk -t arm64-v8a -P 30 build ${CARGO_PROFILE_FLAG} --locked --manifest-path "${CLIENT_DIR}/Cargo.toml"

SO_FILE="${TARGET_DIR}/liblinux_quest_client.so"
if [[ ! -f "${SO_FILE}" ]]; then
    echo "ERROR: Native library not found at ${SO_FILE}" >&2
    exit 1
fi

# Step 2: Prepare packaging directories
OUT_DIR="${CLIENT_DIR}/target/android-apk"
mkdir -p "${OUT_DIR}/lib/arm64-v8a"
cp "${SO_FILE}" "${OUT_DIR}/lib/arm64-v8a/liblinux_quest_client.so"

UNALIGNED_APK="${OUT_DIR}/linux-quest-${BUILD_MODE}-unaligned.apk"
ALIGNED_APK="${OUT_DIR}/linux-quest-${BUILD_MODE}-aligned.apk"
FINAL_APK="${OUT_DIR}/linux-quest-${BUILD_MODE}.apk"

rm -f "${UNALIGNED_APK}" "${ALIGNED_APK}" "${FINAL_APK}"

# Step 3: Package APK with aapt
echo "[2/6] Packaging APK manifest and resources with aapt..."
"${AAPT}" package -F "${UNALIGNED_APK}" \
    -M "${SCRIPT_DIR}/AndroidManifest.xml" \
    -I "${ANDROID_JAR}" \
    -0 so \
    -f

# Compile the tiny Java IME bridge used by NativeActivity to expose an Android InputConnection.
JAVA_OUT="${OUT_DIR}/java-classes"
mkdir -p "${JAVA_OUT}"
javac -source 8 -target 8 -bootclasspath "${ANDROID_JAR}" \
    -d "${JAVA_OUT}" "${SCRIPT_DIR}/src/com/linuxquest/client/ImeBridge.java"
"${D8}" --lib "${ANDROID_JAR}" --min-api 30 --output "${OUT_DIR}" \
    "${JAVA_OUT}/com/linuxquest/client/ImeBridge.class"
cd "${OUT_DIR}"
"${AAPT}" add "${UNALIGNED_APK}" classes.dex
cd "${CLIENT_DIR}"

# Step 4: Add native libraries to APK (uncompressed for direct page-alignment)
echo "[3/6] Adding native shared libraries to APK..."
cd "${OUT_DIR}"
"${AAPT}" add -0 so "${UNALIGNED_APK}" lib/arm64-v8a/liblinux_quest_client.so
cd "${CLIENT_DIR}"

# Step 5: Align APK with zipalign (4-byte alignment, page-align shared libs)
echo "[4/6] Aligning APK with zipalign..."
"${ZIPALIGN}" -f -p 4 "${UNALIGNED_APK}" "${ALIGNED_APK}"

# Step 6: Create debug keystore if absent and sign APK
KEYSTORE="${OUT_DIR}/debug.keystore"
if [[ ! -f "${KEYSTORE}" ]]; then
    echo "[5/6] Generating debug signing keystore..."
    keytool -genkey -v \
        -keystore "${KEYSTORE}" \
        -storepass android \
        -alias androiddebugkey \
        -keypass android \
        -keyalg RSA \
        -keysize 2048 \
        -validity 10000 \
        -dname "CN=Android Debug,O=Android,C=US" > /dev/null 2>&1
else
    echo "[5/6] Using existing debug keystore..."
fi

echo "[6/6] Signing APK with apksigner..."
"${APKSIGNER}" sign \
    --ks "${KEYSTORE}" \
    --ks-pass pass:android \
    --key-pass pass:android \
    --out "${FINAL_APK}" \
    "${ALIGNED_APK}"

"${APKSIGNER}" verify --verbose "${FINAL_APK}" > /dev/null

echo "========================================================"
echo "SUCCESS: Built and signed Quest 3 APK:"
echo "  -> ${FINAL_APK}"
ls -lh "${FINAL_APK}"
echo "========================================================"

# Optional install & run
if [[ "${INSTALL_ON_DEVICE}" == "true" ]]; then
    echo "Installing APK to connected device..."
    "${ADB}" install -r "${FINAL_APK}"
fi

if [[ "${RUN_ON_DEVICE}" == "true" ]]; then
    echo "Launching linux-quest client on Meta Quest..."
    "${ADB}" shell am start -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n com.linuxquest.client/android.app.NativeActivity
fi
