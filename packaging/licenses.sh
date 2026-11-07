#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $(cargo about --version) != 'cargo-about 0.8.4' ]]; then
  echo 'Install the pinned generator: cargo install cargo-about --locked --version 0.8.4' >&2
  exit 1
fi
trap 'rm -f THIRD-PARTY-LICENSES.html.tmp' EXIT
cargo about generate --locked --all-features packaging/licenses.hbs > THIRD-PARTY-LICENSES.html.tmp
# Escape asset text so the HTML contains the complete verbatim licences.
python3 - <<'PY'
from pathlib import Path
import html
p = Path('THIRD-PARTY-LICENSES.html.tmp')
with p.open('a') as out:
    for title, path in [('Inter fonts — SIL Open Font License', 'assets/fonts/INTER-LICENSE.txt'), ('Lucide icons — ISC and Feather MIT', 'assets/icons/LICENSE')]:
        out.write('<h2>' + html.escape(title) + '</h2><pre>' + html.escape(Path(path).read_text()) + '</pre>\n')
    out.write('</html>\n')
p.replace('THIRD-PARTY-LICENSES.html')
PY
