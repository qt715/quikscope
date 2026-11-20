#!/bin/sh
# Rasterises the installer art in assets/branding/installer/ to the 24-bit
# uncompressed BMPs NSIS loads. Requires resvg and ImageMagick 7 (magick); run
# from any directory. The BMPs are committed so Windows builds need neither.
# One file per Windows scaling step: installer.nsi picks the largest one not
# above the system DPI and shows it unscaled, because GDI's bitmap stretching
# blurs or drops pixels.
set -eu
cd "$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)/assets/branding/installer"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for scale in 100 125 150 175 200 225 250 300; do
    for art in sidebar:164:314 header:150:57; do
        name=${art%%:*}
        size=${art#*:}
        width=$(((${size%:*} * scale + 50) / 100))
        height=$(((${size#*:} * scale + 50) / 100))
        resvg --width "$width" --height "$height" "$name.svg" "$tmp/$name.png"
        magick "$tmp/$name.png" -alpha off -type TrueColor "BMP3:$name-$scale.bmp"
    done
done
