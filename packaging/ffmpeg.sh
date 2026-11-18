#!/usr/bin/env bash
# Downloads and verifies the pinned LGPL FFmpeg build (packaging/ffmpeg.env)
# for `linux64` or `win64`, extracts it under DEST (default target/ffmpeg)
# and prints its directory. Build with FFMPEG_DIR=<that directory>; the
# packaging scripts then bundle its shared libraries.
set -euo pipefail
cd "$(dirname "$0")/.."
platform=${1:?usage: packaging/ffmpeg.sh <linux64|win64> [DEST]}
dest=${2:-target/ffmpeg}
# shellcheck source=packaging/ffmpeg.env
source packaging/ffmpeg.env
case "$platform" in
  linux64) name=$FFMPEG_LINUX64 sha=$FFMPEG_LINUX64_SHA256 ;;
  win64) name=$FFMPEG_WIN64 sha=$FFMPEG_WIN64_SHA256 ;;
  *) echo "unknown platform $platform" >&2; exit 2 ;;
esac
mkdir -p "$dest"
archive="$dest/$name"
if ! echo "$sha  $archive" | sha256sum -c --status 2>/dev/null; then
  curl -fsSL --retry 3 -o "$archive" \
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/$FFMPEG_TAG/$name"
  echo "$sha  $archive" | sha256sum -c --quiet
fi
dir="$dest/${name%.tar.xz}"
dir="${dir%.zip}"
if [ ! -d "$dir/lib" ]; then
  case "$name" in
    *.tar.xz) tar -xJf "$archive" -C "$dest" ;;
    *.zip) unzip -q -o "$archive" -d "$dest" ;;
  esac
fi
test -f "$dir/include/libavcodec/avcodec.h"
(cd "$dir" && pwd)
