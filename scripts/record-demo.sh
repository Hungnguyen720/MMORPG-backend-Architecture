#!/usr/bin/env bash
# Record the scripted player (godot/demo_pilot.gd) on a headless X server.
# Needs Xvfb, ffmpeg and a running zone server on 127.0.0.1:5000.
#   scripts/record-demo.sh out.mkv                       # Elderwood: dummy, lag, wolves, loot
#   scripts/record-demo.sh out.mkv --reskin --content "$PWD/tenants/ashfall" \
#       --assets "$PWD/build/ashfall-assets.zip"         # short Ashfall clip
# GODOT defaults to `godot`. Software OpenGL (llvmpipe) gives about 20 fps at 720p.
set -euo pipefail
cd "$(dirname "$0")/.."
out=$(realpath -m "${1:?usage: record-demo.sh out.mkv [client args]}"); shift
Xvfb :99 -screen 0 1280x720x24 -nocursor &
xvfb=$!
sleep 1
DISPLAY=:99 ffmpeg -y -loglevel error -f x11grab -draw_mouse 0 -framerate 30 -video_size 1280x720 -i :99 \
	-c:v libx264 -preset ultrafast -crf 16 "$out" &
ff=$!
DISPLAY=:99 "${GODOT:-godot}" --path godot --rendering-method gl_compatibility \
	--resolution 1280x720 --position 0,0 -- --demo --name Claude "$@" || true
kill -INT $ff; wait $ff || true
kill $xvfb
echo "$out"
