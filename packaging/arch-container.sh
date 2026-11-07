#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cache=${CARGO_TARGET_DIR:-$HOME/.cache/quikscope-arch-target}
mkdir -p "$cache"
docker run --rm -v "$PWD:/work:ro" -v "$cache:/build" archlinux:latest bash -ec '
  pacman -Syu --noconfirm --needed base-devel rust clang cmake pkgconf python namcap vulkan-swrast xorg-server-xvfb dbus
  useradd -m builder
  mkdir /recipe
  cp /work/dist/PKGBUILD /recipe/
  version=$(bash -c "source /recipe/PKGBUILD; echo \$pkgver")
  cp "/work/dist/quikscope-$version-source.tar.gz" "/recipe/quikscope-$version.tar.gz"
  chown -R builder:builder /recipe /build
  cd /recipe
  # Install dependencies with pacman first; makepkg never needs root.
  deps=$(bash -c "source ./PKGBUILD; printf \"%s\\n\" \"\${depends[@]}\" \"\${makedepends[@]}\"")
  pacman -S --noconfirm --needed $deps
  runuser -u builder -- env CARGO_TARGET_DIR=/build makepkg --noconfirm
  namcap PKGBUILD
  namcap ./*.pkg.tar.zst
  pacman -U --noconfirm ./*.pkg.tar.zst
  dbus-run-session -- /work/packaging/smoke-linux.sh
'
