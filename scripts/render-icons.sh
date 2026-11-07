#!/bin/sh
# Requires resvg, ImageMagick 7 (magick), and Python 3; run from any directory.
set -eu
cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
mkdir -p assets/branding/png
for size in 16 24 32 48 64 128 256 512 1024; do
    source=assets/branding/icon.svg
    if [ "$size" = 16 ]; then source=assets/branding/icon-symbolic.svg; fi
    resvg --width "$size" --height "$size" "$source" "assets/branding/png/${size}x${size}.png"
done
# Status-bar template: 18 logical points at Retina density.
resvg --width 36 --height 36 assets/branding/icon-symbolic.svg assets/branding/status-template@2x.png
magick assets/branding/png/16x16.png assets/branding/png/24x24.png \
    assets/branding/png/32x32.png assets/branding/png/48x48.png \
    assets/branding/png/64x64.png assets/branding/png/128x128.png \
    assets/branding/png/256x256.png assets/branding/quikscope.ico
# Modern icns stores PNG payloads directly. Big-endian lengths include headers.
python3 - <<'PY'
from pathlib import Path
from struct import pack
root = Path('assets/branding')
chunks = []
for kind, size in [(b'icp4', 16), (b'icp5', 32), (b'icp6', 64),
                   (b'ic07', 128), (b'ic08', 256), (b'ic09', 512), (b'ic10', 1024)]:
    png = (root / 'png' / f'{size}x{size}.png').read_bytes()
    chunks.append(kind + pack('>I', len(png) + 8) + png)
body = b''.join(chunks)
(root / 'quikscope.icns').write_bytes(b'icns' + pack('>I', len(body) + 8) + body)
PY
resvg assets/branding/banner.svg assets/branding/png/banner.png
