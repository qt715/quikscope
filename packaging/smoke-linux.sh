#!/usr/bin/env bash
# Run inside a disposable container after installing the package with its native
# dependency resolver. No compositor, config or process on the host is touched.
set -euo pipefail
quikscope --version
quikscope --help
if ldd /usr/bin/quikscope | tee /tmp/quikscope-ldd.txt | grep -q 'not found'; then
  echo 'Unresolved runtime shared libraries' >&2; exit 1
fi
if ! command -v Xvfb >/dev/null; then exit 0; fi
export HOME=/tmp/quikscope-smoke-home XDG_RUNTIME_DIR=/tmp/quikscope-smoke-runtime
export XDG_CACHE_HOME="$HOME/.cache"
export DISPLAY=:9 QUIKSCOPE_BACKEND=x11
mkdir -p "$HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
Xvfb :9 -screen 0 800x600x24 &
xvfb=$!
trap 'quikscope quit || true; kill "$xvfb" || true' EXIT
for attempt in {1..100}; do
  if test -S /tmp/.X11-unix/X9; then break; fi
  sleep .1
done
# Preseed onboarding and screenshot output without interacting with a display.
mkdir -p "$HOME/.config/quikscope"
printf 'save_dir = "/tmp/quikscope-smoke-shots"\ncopy_to_clipboard = false\n' > "$HOME/.config/quikscope/config.toml"
log="$XDG_CACHE_HOME/quikscope/daemon.log"
mkdir -p "$(dirname "$log")"
quikscope daemon > "$log" 2>&1 &
daemon=$!
trap 'quikscope quit || true; kill "$daemon" 2>/dev/null || true; kill "$xvfb" || true' EXIT
for attempt in {1..100}; do
  kill -0 "$daemon"
  if grep -q 'daemon ready' "$log"; then break; fi
  sleep .1
done
grep -q 'daemon ready' "$log"
# Opening a real editor also exercises the bundled UI font on minimal images;
# a capture alone never paints text and cannot detect a missing default family.
quikscope capture screen --save --edit --no-notify
find /tmp/quikscope-smoke-shots -name '*.png' -size +0c -print -quit | grep -q .
# Keep checking through initial frame rendering, not only the IPC acknowledgement.
for frame in {1..20}; do
  # ping can spawn a replacement daemon; also require the original PID to live.
  kill -0 "$daemon"
  quikscope ping
  kill -0 "$daemon"
  if grep -Ei 'panicked at|thread .*panicked' "$log"; then
    echo 'Daemon panicked while opening the editor' >&2
    exit 1
  fi
  sleep .1
done
