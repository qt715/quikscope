#!/usr/bin/env bash
# Build and test dependencies for Ubuntu 24.04 CI runners.
# Codecs are compiled from source by their crates: CMake builds libopus, the
# C/C++ compilers build OpenH264, libwebp, zstd and VMA, and NASM gives
# OpenH264 its SIMD paths. clang (libclang) is for the PipeWire bindings.
set -euo pipefail
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential clang cmake nasm pkg-config \
  libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libfontconfig-dev libfreetype-dev libssl-dev libzstd-dev libasound2-dev \
  libgbm-dev libpipewire-0.3-dev libpulse-dev \
  libvulkan1 mesa-vulkan-drivers
# Test-only: some UI tests make fixture clips with the ffmpeg CLI and check
# exported files with ffprobe. quikscope itself does not use FFmpeg.
sudo apt-get install -y --no-install-recommends ffmpeg
