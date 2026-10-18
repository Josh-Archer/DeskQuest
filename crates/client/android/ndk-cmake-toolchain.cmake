# CMake toolchain used by build.sh for crates that build C/C++ with cmake for
# aarch64-linux-android (the Khronos OpenXR loader from openxr-sys, and Opus).
#
# - Points CMake at the NDK ($ANDROID_NDK_HOME), which the plain cmake-rs
#   "-DCMAKE_SYSTEM_NAME=Android" setup cannot find on its own.
# - Defines CMAKE_INSTALL_INCLUDEDIR: OpenXR-SDK only includes GNUInstallDirs on Android when
#   INSTALL_TO_ARCHITECTURE_PREFIXES is set, so otherwise its header install step targets "/openxr".
set(ANDROID_ABI arm64-v8a)
set(ANDROID_PLATFORM android-30)
set(CMAKE_INSTALL_INCLUDEDIR include)
include("$ENV{ANDROID_NDK_HOME}/build/cmake/android.toolchain.cmake")
