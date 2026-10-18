#!/bin/sh
# Regenerates crates/encoder/tests/fixtures/libva-2.20.0-abi.txt from the libva headers.
#
# Usage: scripts/libva-abi/regen.sh [INCLUDE_DIR]
#
# INCLUDE_DIR must contain va/va.h and friends. Without it, the script downloads
# libva-dev with apt-get and extracts it with dpkg -x into a temporary directory.
# The checked-in fixture was made from libva-dev 2.20.0-2ubuntu0.2 (Ubuntu noble).
# If the headers you regenerate from differ, the fixture test in
# crates/encoder/src/vaapi/abi.rs fails, which is the intended check.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

if [ $# -ge 1 ]; then
    inc=$1
else
    (cd "$work" && apt-get download libva-dev)
    mkdir "$work/root"
    dpkg -x "$work"/libva-dev_*.deb "$work/root"
    inc="$work/root/usr/include"
fi

cc -std=c11 -Wall -I"$inc" -o "$work/probe" "$here/abi_probe.c"
out="$root/crates/encoder/tests/fixtures/libva-2.20.0-abi.txt"
{
    echo "# libva ABI fixture: output of scripts/libva-abi/abi_probe.c."
    echo "# Built against the libva headers used by regen.sh. The checked-in copy is from"
    echo "# libva-dev 2.20.0-2ubuntu0.2 (Ubuntu noble). Regenerate with scripts/libva-abi/regen.sh."
    echo "# Lines: KIND KEY VALUE (C = constant, S = sizeof, O = offsetof, B = bit position)."
    "$work/probe"
} > "$out"
echo "wrote $out"
