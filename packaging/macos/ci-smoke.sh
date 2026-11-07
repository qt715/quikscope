#!/bin/bash
# Hosted disposable macOS runner only. Never run against a personal TCC database.
set -euo pipefail
[[ "${GITHUB_ACTIONS:-}" == true && "${RUNNER_OS:-}" == macOS ]] || {
    echo 'This script changes runner TCC records and only runs in GitHub Actions on macOS.' >&2
    exit 2
}
app="${1:?usage: ci-smoke.sh /absolute/path/quikscope.app}"
[[ "$app" == /* ]] || { echo 'App path must be absolute' >&2; exit 2; }
binary="$app/Contents/MacOS/quikscope"
artifacts="${RUNNER_TEMP:?}/quikscope-smoke"
mkdir -p "$artifacts"
exec > >(tee "$artifacts/smoke.log") 2>&1

# A valid signed requirement is recorded for both Launch Services' bundle
# identity and a directly launched CLI's absolute executable identity.
requirement="$(codesign -d -r- "$app" 2>&1)"
requirement="${requirement##*designated => }"
printf '%s' "$requirement" | csreq -r- -b "$artifacts/identity.csreq"
requirement_hex="$(xxd -p "$artifacts/identity.csreq" | tr -d '\n')"
tcc='/Library/Application Support/com.apple.TCC/TCC.db'
for identity in io.github.qt715.quikscope "$binary"; do
    client_type=0
    [[ "$identity" == /* ]] && client_type=1
    # Quote SQL literals even though runner paths normally have no apostrophes.
    escaped="${identity//\'/\'\'}"
    echo "Attempting runner-only TCC ScreenCapture allow: client=$identity client_type=$client_type auth_value=2 auth_reason=4"
    if ! sudo sqlite3 "$tcc" "INSERT OR REPLACE INTO access
        (service, client, client_type, auth_value, auth_reason, auth_version,
         csreq, indirect_object_identifier_type, indirect_object_identifier, flags, last_modified)
        VALUES ('kTCCServiceScreenCapture', '$escaped', $client_type, 2, 4, 1,
         X'$requirement_hex', 0, 'UNUSED', 0, strftime('%s','now'));"; then
        echo '::warning::Hosted runner refused TCC database grant; capture will still be attempted and must succeed.'
    fi
done
sudo sqlite3 "$tcc" "SELECT service,client,client_type,auth_value,auth_reason FROM access WHERE service='kTCCServiceScreenCapture';" \
    > "$artifacts/tcc.log" 2>&1 || true

# Keep the runner account's real HOME/TCC identity for grants above. Only the
# daemon and clients use the isolated HOME; no onboarding or persisted user state.
export HOME="$RUNNER_TEMP/quikscope-smoke-home"
export QUIKSCOPE_SOCKET="$HOME/Library/Caches/quikscope/ipc.sock"
mkdir -p "$HOME/Library/Application Support/quikscope" "$HOME/Library/Caches/quikscope" "$artifacts/screenshots"
cat > "$HOME/Library/Application Support/quikscope/config.toml" <<CONFIG
save_dir = "$artifacts/screenshots"
filename = "screenshot_%Y%m%d_%H%M%S.png"
copy_to_clipboard = true
notify = true
open_editor = false
cursor = false
freeze = true
notification_timeout_ms = 8000

[editor]
color = "#e03131"
stroke = 1
close_on_save = false
font_family = "Inter"
font_weight = 600
font_size = 28.0
font_italic = false

[ocr]
languages = []

[hotkeys]
region = "Cmd+Shift+2"
screen = "Cmd+Shift+8"
window = "Cmd+Shift+7"
open_last = "Cmd+Shift+E"
CONFIG
QUIKSCOPE_LOG=quikscope=debug "$binary" daemon > "$artifacts/daemon.log" 2>&1 &
daemon_pid=$!
cleanup() {
    "$binary" quit > "$artifacts/quit.log" 2>&1 || true
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
}
trap cleanup EXIT
for ((attempt=0; attempt<60; attempt++)); do
    [[ -S "$QUIKSCOPE_SOCKET" ]] && break
    kill -0 "$daemon_pid" || { cat "$artifacts/daemon.log"; exit 1; }
    sleep 1
done
[[ -S "$QUIKSCOPE_SOCKET" ]] || { echo 'Daemon socket did not appear'; exit 1; }
"$binary" ping
"$binary" capture screen --save --no-notify --no-edit | tee "$artifacts/capture.log"
shopt -s nullglob
screenshots=("$artifacts/screenshots/"*.png)
[[ ${#screenshots[@]} -gt 0 ]] || { echo 'Capture produced no PNG'; exit 1; }
for screenshot in "${screenshots[@]}"; do
    sips --getProperty pixelWidth --getProperty pixelHeight "$screenshot"
done
# Reading promised pasteboard data from a separate process exercises the
# daemon's main-runloop provider, including the TIFF compatibility conversion.
xcrun swift - "${screenshots[0]}" "$artifacts/clipboard.png" <<'SWIFT'
import AppKit
import Foundation

let pasteboard = NSPasteboard.general
guard let png = pasteboard.data(forType: .png),
      let clipboard = NSBitmapImageRep(data: png),
      let savedData = try? Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1])),
      let saved = NSBitmapImageRep(data: savedData),
      clipboard.pixelsWide == saved.pixelsWide,
      clipboard.pixelsHigh == saved.pixelsHigh,
      clipboard.samplesPerPixel == saved.samplesPerPixel,
      let clipboardPixels = clipboard.bitmapData,
      let savedPixels = saved.bitmapData else {
    fatalError("PNG pasteboard data is missing or differs in shape from saved capture")
}
for row in 0..<saved.pixelsHigh {
    let count = saved.pixelsWide * saved.samplesPerPixel
    guard memcmp(clipboardPixels + row * clipboard.bytesPerRow,
                 savedPixels + row * saved.bytesPerRow, count) == 0 else {
        fatalError("PNG pasteboard pixels differ from saved screenshot at row \(row)")
    }
}
guard let tiff = pasteboard.data(forType: .tiff),
      let compatibility = NSBitmapImageRep(data: tiff),
      compatibility.pixelsWide == saved.pixelsWide,
      compatibility.pixelsHigh == saved.pixelsHigh else {
    fatalError("TIFF pasteboard representation is missing or has incorrect dimensions")
}
try png.write(to: URL(fileURLWithPath: CommandLine.arguments[2]))
print("PNG pasteboard pixels match saved capture; TIFF representation is available")
SWIFT

# Accessory applications must still be able to present a key editor window.
"$binary" open-last
for ((attempt=0; attempt<60; attempt++)); do
    grep -q 'macOS editor key window: true' "$artifacts/daemon.log" && break
    kill -0 "$daemon_pid" || { cat "$artifacts/daemon.log"; exit 1; }
    sleep 1
done
"$binary" capture screen --save --no-notify --no-edit
grep -q 'macOS editor key window: true' "$artifacts/daemon.log" || {
    cat "$artifacts/daemon.log"
    echo 'Editor did not become a native key window'
    exit 1
}
echo 'Editor became a native key window'
xcrun swift - "$daemon_pid" <<'SWIFT'
import AppKit
import Foundation
guard let pid = Int32(CommandLine.arguments[1]),
      let app = NSRunningApplication(processIdentifier: pid) else {
    fatalError("Daemon application disappeared")
}
print("Daemon activation policy: \(app.activationPolicy.rawValue); active: \(app.isActive)")
precondition(app.activationPolicy == .accessory, "Daemon must not appear in the Dock")
precondition(app.isActive, "Accessory editor did not activate")
SWIFT
