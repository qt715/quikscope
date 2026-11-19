#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $(cargo about --version) != 'cargo-about 0.8.4' ]]; then
  echo 'Install the pinned generator: cargo install cargo-about --locked --version 0.8.4' >&2
  exit 1
fi
trap 'rm -f THIRD-PARTY-LICENSES.html.tmp THIRD-PARTY-LICENSES.metadata.tmp' EXIT
cargo about generate --locked --all-features packaging/licenses.hbs > THIRD-PARTY-LICENSES.html.tmp
cargo metadata --locked --all-features --format-version 1 > THIRD-PARTY-LICENSES.metadata.tmp
python3 - <<'PY'
from pathlib import Path
import html, json, sys

def vma(text):
    # The crate ships no VMA licence file; the notice heads the header.
    lines = []
    for line in text.splitlines():
        if not line.startswith('//'):
            break
        lines.append(line[3:] if line.startswith('// ') else line[2:])
    return '\n'.join(lines).strip() + '\n'

# cargo-about reports crate licences only. These C/C++ libraries are compiled
# into the executable by their -sys crates; read their notices from the locked
# crate sources so the texts match what is built.
NATIVE = [
    ('libopus (BSD-3-Clause)', 'opusic-sys', 'opus/COPYING', str),
    ('OpenH264 (BSD-2-Clause)', 'openh264-sys2', 'upstream/LICENSE', str),
    ('libwebp (BSD-3-Clause)', 'libwebp-sys2', 'c_src/COPYING', str),
    ('libwebp additional IP rights grant (patents)', 'libwebp-sys2', 'c_src/PATENTS', str),
    ('Zstandard (BSD-3-Clause)', 'zstd-sys', 'zstd/LICENSE', str),
    ('Vulkan Memory Allocator (MIT)', 'vk-mem', 'vendor/VulkanMemoryAllocator/include/vk_mem_alloc.h', vma),
]
# Rust code compiled into quikscope's own crate, which cargo-about cannot see.
VENDORED = [
    ('zune-jpegxl 0.5.2, modified (Zlib)', 'vendor/zune-jpegxl/LICENSE-ZLIB'),
    ('zune-jpegxl modifications notice', 'vendor/zune-jpegxl/NOTICE'),
]
ASSETS = [
    ('Inter fonts — SIL Open Font License', 'assets/fonts/INTER-LICENSE.txt'),
    ('Lucide icons — ISC and Feather MIT', 'assets/icons/LICENSE'),
]

packages = json.loads(Path('THIRD-PARTY-LICENSES.metadata.tmp').read_text())['packages']
p = Path('THIRD-PARTY-LICENSES.html.tmp')
# Escape the texts so the HTML contains the complete verbatim licences.
with p.open('a') as out:
    out.write('<h1>Bundled C and C++ libraries</h1>\n')
    for title, crate, file, extract in NATIVE:
        versions = sorted((pkg for pkg in packages if pkg['name'] == crate), key=lambda pkg: pkg['version'])
        if not versions:
            sys.exit(f'{crate} left Cargo.lock: update NATIVE in packaging/licenses.sh')
        texts = {}
        for pkg in versions:
            path = Path(pkg['manifest_path']).parent / file
            if not path.is_file():
                sys.exit(f'{path} is missing: update NATIVE in packaging/licenses.sh')
            texts.setdefault(extract(path.read_text()), []).append(f"{crate} {pkg['version']}")
        for text, sources in texts.items():
            out.write('<h2>' + html.escape(title) + '</h2><p>Built from the source in '
                      + html.escape(', '.join(sources)) + '.</p><pre>' + html.escape(text) + '</pre>\n')
    out.write('<h1>Vendored Rust code</h1>\n')
    for title, path in VENDORED:
        out.write('<h2>' + html.escape(title) + '</h2><pre>' + html.escape(Path(path).read_text()) + '</pre>\n')
    out.write('<h1>Bundled assets</h1>\n')
    for title, path in ASSETS:
        out.write('<h2>' + html.escape(title) + '</h2><pre>' + html.escape(Path(path).read_text()) + '</pre>\n')
    out.write('</html>\n')
p.replace('THIRD-PARTY-LICENSES.html')
PY
