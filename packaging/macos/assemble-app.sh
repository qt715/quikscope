#!/bin/bash
# Usage: assemble-app.sh [release-executable] [output-app]
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
binary="${1:-$root/target/release/quikscope}"
app="${2:-$root/dist/quikscope.app}"
if [[ $# -gt 2 || "$app" != *.app ]]; then
    echo "usage: $0 [release-executable] [output.app]" >&2
    exit 2
fi
[[ -f "$binary" && -x "$binary" ]] || { echo "Missing executable: $binary" >&2; exit 1; }
[[ -f "$root/assets/branding/quikscope.icns" ]] || { echo 'Missing assets/branding/quikscope.icns' >&2; exit 1; }
[[ ! -e "$app" ]] || { echo "Output already exists: $app" >&2; exit 1; }

# Do not run the supplied binary: a universal/cross-built executable may not
# run on the packaging host. Cargo's package version is the bundle version.
version=""
while IFS= read -r line; do
    [[ "$line" == '[package]' ]] && in_package=1 && continue
    if [[ "${in_package:-0}" == 1 ]]; then
        [[ "$line" == \[* ]] && break
        if [[ "$line" =~ ^version[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
            version="${BASH_REMATCH[1]}"
            break
        fi
    fi
done < "$root/Cargo.toml"
[[ -n "$version" ]] || { echo 'Cannot read Cargo package version' >&2; exit 1; }

mkdir -p "$(dirname "$app")"
mkdir "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/Licenses"
cp "$binary" "$app/Contents/MacOS/quikscope"
chmod 755 "$app/Contents/MacOS/quikscope"
cp "$root/packaging/macos/Info.plist" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$app/Contents/Info.plist"
cp "$root/assets/branding/quikscope.icns" "$app/Contents/Resources/quikscope.icns"
for license in LICENSE-MIT LICENSE-APACHE THIRD-PARTY-LICENSES.html; do
    if [[ -f "$root/$license" ]]; then
        cp "$root/$license" "$app/Contents/Resources/Licenses/"
    fi
done
cp "$root/assets/fonts/INTER-LICENSE.txt" "$app/Contents/Resources/Licenses/"
cp "$root/assets/icons/LICENSE" "$app/Contents/Resources/Licenses/ICONS-LICENSE"
# FFmpeg (packaging/macos/build-ffmpeg.sh, possibly lipo-merged): the binary
# loads @rpath/lib<name>.<major>.dylib with rpath @executable_path/../Frameworks.
ffmpeg_lib="${FFMPEG_LIB:?set FFMPEG_LIB to the FFmpeg lib directory to bundle}"
mkdir -p "$app/Contents/Frameworks"
dylibs=()
for dylib in "$ffmpeg_lib"/lib*.dylib; do
    [[ "$(basename "$dylib")" =~ ^lib[a-z]+\.[0-9]+\.dylib$ ]] && dylibs+=("$dylib")
done
[[ ${#dylibs[@]} -eq 7 ]] || { echo "Expected 7 FFmpeg dylibs in $ffmpeg_lib, found ${#dylibs[@]}" >&2; exit 1; }
for dylib in "${dylibs[@]}"; do
    cp -L "$dylib" "$app/Contents/Frameworks/"
    codesign --force --sign - "$app/Contents/Frameworks/$(basename "$dylib")"
done
cp "$ffmpeg_lib/../LICENSE.txt" "$app/Contents/Resources/Licenses/FFMPEG-LICENSE.txt"
# Ad-hoc signing gives local/CI bundles a coherent identity. Distribution
# signing and notarization are separate release operations.
codesign --force --sign - --identifier io.github.qt715.quikscope "$app"
printf '%s\n' "$app"
