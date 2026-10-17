#!/bin/sh
# Regenerates crates/display/tests/fixtures/evdi-<pin>-abi.txt from the real headers.
#
# usage: regen.sh <evdi_drm.h> <linux-libc-dev-include-root> <evdi-commit> <libc-dev-version> <out-file>
#
#   <evdi_drm.h>                 module/evdi_drm.h from DisplayLink/evdi at <evdi-commit>, saved
#                                in a fresh empty directory
#   <linux-libc-dev-include-root> usr/include of `dpkg -x linux-libc-dev_*.deb <fresh-dir>`
#
# Nothing is executed from the header directories; the compiler only reads them.
set -eu

evdi_h=$1
inc_root=$2
pin=$3
libc_ver=$4
out=$5

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

evdi_dir=$(dirname "$evdi_h")
${CC:-cc} -Wall -Wextra -o "$work/abi_probe" \
    -I "$evdi_dir" -I "$inc_root" -I "$inc_root/x86_64-linux-gnu" \
    "$here/abi_probe.c"

{
    echo "# EVDI userspace ABI as computed by a C compiler (scripts/evdi-abi/abi_probe.c)."
    echo "# evdi_drm.h: DisplayLink/evdi module/evdi_drm.h at commit $pin"
    echo "# drm/drm.h + asm ioctl headers: Ubuntu linux-libc-dev $libc_ver"
    echo "# target: x86_64 LP64, gcc $(${CC:-cc} -dumpversion)"
    "$work/abi_probe"
} >"$out"
