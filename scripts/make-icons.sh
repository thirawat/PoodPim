#!/bin/sh
# Regenerates the app icons from assets/logo.svg (needs rsvg-convert: brew install librsvg):
#   assets/AppIcon.icns  macOS app icon (scripts/bundle-macos.sh copies it into the app)
#   assets/PoodPim.ico   Windows Start menu shortcut and Settings > Apps icon
#   assets/icon-64.rgba  the Settings window's icon (64×64 raw RGBA, needs ffmpeg)
# The results are committed, so building the app doesn't need rsvg-convert.
set -eu
cd "$(dirname "$0")/.."
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

SET=$TMP/AppIcon.iconset
mkdir -p "$SET"
for s in 16 32 128 256 512; do
  rsvg-convert -w $s -h $s assets/logo.svg -o "$SET/icon_${s}x${s}.png"
  rsvg-convert -w $((s * 2)) -h $((s * 2)) assets/logo.svg -o "$SET/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$SET" -o assets/AppIcon.icns

rsvg-convert -w 256 -h 256 assets/logo.svg -o "$TMP/ico.png"
python3 -c "
from PIL import Image
import sys
Image.open(sys.argv[1]).save(sys.argv[2], sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
" "$TMP/ico.png" assets/PoodPim.ico
rsvg-convert -w 64 -h 64 assets/logo.svg -o "$TMP/64.png"
ffmpeg -loglevel error -y -i "$TMP/64.png" -f rawvideo -pix_fmt rgba assets/icon-64.rgba
echo "Built assets/AppIcon.icns, assets/PoodPim.ico and assets/icon-64.rgba"
