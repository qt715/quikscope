#!/usr/bin/env bash
# Builds the LGPL FFmpeg shared libraries the macOS app links and bundles,
# for one architecture: packaging/macos/build-ffmpeg.sh <arm64|x86_64> PREFIX
#
# No GPL code and no third-party codec libraries: H.264/HEVC encode through
# VideoToolbox, AAC through FFmpeg's encoder, GIF/FFV1 natively. WebP, AVIF
# and JPEG XL encoders need external libraries and are left out on macOS
# (the app reports them as unavailable). Libraries use @rpath install names
# so the app loads them from Contents/Frameworks.
set -euo pipefail
arch=${1:?usage: build-ffmpeg.sh <arm64|x86_64> PREFIX}
prefix=${2:?usage: build-ffmpeg.sh <arm64|x86_64> PREFIX}
version=9.0.2
sha256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL --retry 3 -o "$work/ffmpeg.tar.xz" "https://ffmpeg.org/releases/ffmpeg-$version.tar.xz"
echo "$sha256  $work/ffmpeg.tar.xz" | shasum -a 256 -c --quiet
tar -xJf "$work/ffmpeg.tar.xz" -C "$work"
cd "$work/ffmpeg-$version"
cross=()
if [ "$arch" != "$(uname -m)" ]; then
  cross=(--enable-cross-compile --target-os=darwin)
fi
./configure \
  --prefix="$prefix" --arch="$arch" --cc="clang -arch $arch" "${cross[@]}" \
  --extra-cflags="-mmacosx-version-min=${MACOSX_DEPLOYMENT_TARGET:-12.3}" \
  --extra-ldflags="-mmacosx-version-min=${MACOSX_DEPLOYMENT_TARGET:-12.3}" \
  --install-name-dir='@rpath' \
  --enable-shared --disable-static --disable-programs --disable-doc \
  --disable-autodetect --enable-videotoolbox --enable-audiotoolbox \
  --enable-pthreads --enable-zlib
make -j"$(sysctl -n hw.ncpu)"
make install
install -m644 COPYING.LGPLv2.1 "$prefix/LICENSE.txt"
