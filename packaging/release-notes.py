#!/usr/bin/env python3
"""Extract the release version's changelog section; never fabricate notes."""
import os
from pathlib import Path
import re
import tomllib

version = tomllib.loads(Path('Cargo.toml').read_text())['package']['version']
text = Path('CHANGELOG.md').read_text()
sections = re.split(r'(?m)^## ', text)[1:]
for section in sections:
    heading, _, body = section.partition('\n')
    if re.search(r'(?<![\d.])' + re.escape(version) + r'(?![\d.])', heading):
        print(body.strip())
        break
else:
    # Dispatch can exercise packaging against an unreleased development tree.
    if os.environ.get('GITHUB_REF', '').startswith('refs/tags/'):
        raise SystemExit(f'No CHANGELOG section for {version}')
    print(sections[0].partition('\n')[2].strip() if sections else text.strip())
