#!/bin/sh
# Requires resvg and ImageMagick 7 (magick); run from any directory.
set -eu
cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
mkdir -p assets/branding/png
for size in 16 24 32 48 64 128 256 512 1024; do
    source=assets/branding/icon.svg
    if [ "$size" = 16 ]; then source=assets/branding/icon-symbolic.svg; fi
    resvg --width "$size" --height "$size" "$source" "assets/branding/png/${size}x${size}.png"
done
resvg --width 32 --height 32 assets/branding/icon-symbolic.svg assets/branding/tray-idle.png
resvg --width 32 --height 32 assets/branding/tray-recording.svg assets/branding/tray-recording.png
magick assets/branding/png/16x16.png assets/branding/png/24x24.png \
    assets/branding/png/32x32.png assets/branding/png/48x48.png \
    assets/branding/png/64x64.png assets/branding/png/128x128.png \
    assets/branding/png/256x256.png assets/branding/quikscope.ico
resvg assets/branding/banner.svg assets/branding/png/banner.png
