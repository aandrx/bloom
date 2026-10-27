#!/usr/bin/env python3
"""Validate the committed third-party notice bundle and its references."""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BUNDLE = ROOT / 'third-party-notices'
REF = re.compile(r'`((?:licenses|fallbacks)/[^`]+)`')


def rows(manifest):
    values = []
    for line in manifest.read_text(encoding='utf-8').splitlines():
        if line.startswith('| ') and not line.startswith('| Package ') \
                and not line.startswith('| --- '):
            values.append(line)
    return values


def main():
    failures = []
    for name, minimum in (('rust-manifest.md', 400), ('dart-manifest.md', 40)):
        path = BUNDLE / name
        if not path.is_file():
            failures.append(f'missing {name}')
            continue
        manifest_rows = rows(path)
        if len(manifest_rows) < minimum:
            failures.append(f'{name}: only {len(manifest_rows)} rows')
        for line in manifest_rows:
            refs = REF.findall(line)
            if not refs:
                failures.append(f'{name}: row has no notice reference: {line[:120]}')
                continue
            for ref in refs:
                if not (BUNDLE / ref).is_file():
                    failures.append(f'{name}: missing referenced file {ref}')
    for name in ('README.md', 'MIT.txt', 'Apache-2.0.txt', 'MPL-2.0.txt'):
        path = BUNDLE / 'fallbacks' / name
        if not path.is_file() or path.stat().st_size == 0:
            failures.append(f'missing or empty fallback {name}')
    fallback_readme = BUNDLE / 'fallbacks' / 'README.md'
    if fallback_readme.is_file() and len(rows(fallback_readme)) < 40:
        failures.append('fallbacks/README.md lists too few packages')
    readme = BUNDLE / 'README.md'
    if not readme.is_file():
        failures.append('missing README.md')
    for path in BUNDLE.rglob('*'):
        if path.is_file():
            text = path.read_text(encoding='utf-8', errors='replace')
            if '/home/' in text or 'file://' in text:
                failures.append(f'{path.relative_to(BUNDLE)} contains an absolute path')
    if failures:
        for failure in failures[:20]:
            print(f'test-third-party-notices: {failure}', file=sys.stderr)
        return 1
    print(f'test-third-party-notices: OK ({len(rows(BUNDLE / "rust-manifest.md"))} Rust, '
          f'{len(rows(BUNDLE / "dart-manifest.md"))} Dart packages)')
    return 0


if __name__ == '__main__':
    sys.exit(main())
