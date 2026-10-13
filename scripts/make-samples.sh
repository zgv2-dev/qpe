#!/usr/bin/env bash
# Generate small test clips in ./samples (or $1).
set -euo pipefail
out=${1:-samples}
mkdir -p "$out"
src=(-f lavfi -i testsrc2=size=1280x720:rate=30:duration=10 -f lavfi -i sine=frequency=440:duration=10)
ff() { ffmpeg -hide_banner -v error -y "$@"; }
ff "${src[@]}" -c:v libx264 -g 60 -c:a aac -shortest "$out/h264.mp4"
ff "${src[@]}" -c:v libx265 -g 60 -tag:v hvc1 -c:a aac -shortest "$out/hevc.mkv"
ff "${src[@]}" -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -b:v 1M -c:a libopus -shortest "$out/vp9.mkv"
ff -display_rotation 90 -i "$out/h264.mp4" -c copy "$out/rotated.mp4"
ls -lh "$out"
