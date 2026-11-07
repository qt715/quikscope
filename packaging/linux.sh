#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
version=${VERSION:-$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')}
binary=${1:-${CARGO_TARGET_DIR:-target}/release/quikscope}
for path in "$binary" THIRD-PARTY-LICENSES.html assets/branding/png/256x256.png; do
  test -f "$path" || { echo "Missing release input: $path (build binary, generate licences, merge branding)" >&2; exit 1; }
done
mkdir -p dist
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
install -Dm755 "$binary" "$stage/usr/bin/quikscope"
install -Dm644 packaging/quikscope.desktop "$stage/usr/share/applications/quikscope.desktop"
install -Dm644 packaging/quikscope.service "$stage/usr/lib/systemd/user/quikscope.service"
for size in 16 24 32 48 64 128 256 512; do
  install -Dm644 "assets/branding/png/${size}x${size}.png" "$stage/usr/share/icons/hicolor/${size}x${size}/apps/quikscope.png"
done
for file in LICENSE-MIT LICENSE-APACHE THIRD-PARTY-LICENSES.html assets/fonts/INTER-LICENSE.txt assets/icons/LICENSE; do
  install -Dm644 "$file" "$stage/usr/share/licenses/quikscope/$(basename "$file")"
done
for file in README.md CHANGELOG.md docs/*.md; do
  install -Dm644 "$file" "$stage/usr/share/doc/quikscope/$(basename "$file")"
done
# Debian's scanner resolves DT_NEEDED into distro/version-specific package names.
mkdir -p "$stage/debian"
printf 'Source: quikscope\nSection: graphics\nPriority: optional\nMaintainer: quikscope contributors <212748636+qt715@users.noreply.github.com>\n\nPackage: quikscope\nArchitecture: amd64\nDescription: screenshot daemon and annotation editor\n' > "$stage/debian/control"
debdeps=$(cd "$stage" && dpkg-shlibdeps -O usr/bin/quikscope | cut -d= -f2-)
# RPM records ELF soname and GLIBC symbol requirements, rather than guessing
# Fedora/openSUSE package names. The binary is identical to the .deb binary.
rpmrequires=$(printf '%s\n' "$stage/usr/bin/quikscope" | /usr/lib/rpm/find-requires)
python3 - "$stage" "$version" "$debdeps" "$rpmrequires" <<'PY'
import json, pathlib, sys
stage, version, debdeps, rpmdeps = sys.argv[1:]
# JSON is a YAML subset accepted by nFPM.
config = dict(name='quikscope', arch='amd64', platform='linux', version=version,
    maintainer='quikscope contributors <212748636+qt715@users.noreply.github.com>',
    description='Screenshot daemon and GPUI annotation editor', homepage='https://github.com/qt715/quikscope',
    license='MIT OR Apache-2.0', section='graphics', priority='optional',
    contents=[dict(src=stage+'/usr/', dst='/usr/', type='tree')],
    overrides=dict(deb=dict(depends=debdeps.split(', ') + ['libxkbcommon0', 'libwayland-client0', 'libvulkan1'],
        recommends=['libtesseract5', 'tesseract-ocr-eng', 'xdg-desktop-portal']),
        rpm=dict(depends=rpmdeps.splitlines() + ['libxkbcommon.so.0()(64bit)', 'libwayland-client.so.0()(64bit)', 'libvulkan.so.1()(64bit)'],
        recommends=['tesseract', 'xdg-desktop-portal'])))
pathlib.Path('dist/nfpm.json').write_text(json.dumps(config))
PY
nfpm package --config dist/nfpm.json --packager deb --target "dist/quikscope_${version}_amd64.deb"
nfpm package --config dist/nfpm.json --packager rpm --target "dist/quikscope-${version}.x86_64.rpm"
rm dist/nfpm.json
