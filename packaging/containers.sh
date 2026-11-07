#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# dist must contain artifacts from the native Ubuntu release runner, not a
# binary built on a newer host glibc.
for image in debian:trixie ubuntu:24.04; do
  docker run --rm -v "$PWD:/work:ro" "$image" bash -ec '
    apt-get update
    apt-get install -y /work/dist/*.deb xvfb dbus-x11 mesa-vulkan-drivers
    dbus-run-session -- /work/packaging/smoke-linux.sh
  '
done
docker run --rm -v "$PWD:/work:ro" fedora:latest bash -ec '
  dnf install -y /work/dist/*.rpm xorg-x11-server-Xvfb dbus-daemon mesa-vulkan-drivers
  dbus-run-session -- /work/packaging/smoke-linux.sh
'
if [[ ${QUIKSCOPE_TEST_OPENSUSE:-0} == 1 ]]; then
  docker run --rm -v "$PWD:/work:ro" opensuse/tumbleweed:latest bash -ec '
    zypper --non-interactive --no-gpg-checks install --allow-unsigned-rpm /work/dist/*.rpm xorg-x11-server-Xvfb dbus-1 Mesa-vulkan-device-select
    dbus-run-session -- /work/packaging/smoke-linux.sh
  '
fi
