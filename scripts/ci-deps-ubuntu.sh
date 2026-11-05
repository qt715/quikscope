#!/usr/bin/env bash
# Build and test dependencies for Ubuntu 24.04 CI runners.
set -euo pipefail
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  clang pkg-config \
  libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libfontconfig-dev libfreetype-dev libssl-dev libzstd-dev libasound2-dev \
  libvulkan1 mesa-vulkan-drivers
