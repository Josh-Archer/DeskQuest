#!/bin/sh
# Independent check of the HEVC VPS/SPS/PPS bits this crate writes for packed headers.
# It dumps the parameter sets with a test hook, then asks ffprobe to parse them and
# prints the colour fields it finds. Expected: bt709 primaries and transfer, bt709
# matrix, tv (limited) range.
#
# Usage: scripts/libva-abi/check_hevc_vui.sh
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
LINUX_QUEST_DUMP_HEVC_PS="$work/ps.h265" cargo test -q -p linux-quest-encoder dump_hevc_parameter_sets_for_ffprobe --manifest-path "$root/Cargo.toml"
ffprobe -v error -show_streams -of default=noprint_wrappers=1 "$work/ps.h265" \
    | grep -E 'codec_name|width|height|color_(space|transfer|primaries|range)'
