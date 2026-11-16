#!/bin/sh
# Rebuilds assets/demo.gif (the README demo) from assets/demo/scene.html.
# Needs Google Chrome, ffmpeg, and puppeteer-core installed in the folder you run this from:
#   mkdir /tmp/demo && cd /tmp/demo && npm i puppeteer-core@24.10.0
#   /path/to/repo/scripts/make-demo.sh
set -eu
REPO=$(cd "$(dirname "$0")/.." && pwd)
FRAMES=$(mktemp -d)
trap 'rm -rf "$FRAMES"' EXIT
node "$REPO/scripts/make-demo.mjs" "$FRAMES" 15
ffmpeg -loglevel error -y -framerate 15 -i "$FRAMES/f%04d.png" \
  -vf "split[a][b];[a]palettegen=max_colors=192:stats_mode=diff[p];[b][p]paletteuse=dither=sierra2_4a:diff_mode=rectangle" \
  -loop 0 "$REPO/assets/demo.gif"
echo "Built assets/demo.gif ($(du -h "$REPO/assets/demo.gif" | cut -f1))"
