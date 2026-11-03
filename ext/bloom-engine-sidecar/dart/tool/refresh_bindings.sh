#!/bin/sh
# Refresh generated Dart bindings from an OpenBubbles tag checkout.
#
# The generated tree is replaced wholesale, but local attribution files
# (LICENSE and NOTICE) live beside it and must survive every refresh
# byte-for-byte. They are copied into a staging tree and the whole directory is
# swapped in, so an interrupted copy never leaves a half-written upstream tree.
#
# Usage:
#   refresh_bindings.sh /path/to/openbubbles-tag-checkout
#   refresh_bindings.sh --notice-current
#
# The second form re-applies the modified-distribution notice to the current
# local tree without changing any generated body bytes. Both forms add the same
# idempotent notice comment to every *.dart file in the staged tree.
set -eu

usage='usage: refresh_bindings.sh /path/to/openbubbles-tag-checkout
       refresh_bindings.sh --notice-current'
mode=refresh
if [ "${1:-}" = "--notice-current" ]; then
  [ "$#" -eq 1 ] || { echo "$usage" >&2; exit 2; }
  mode=notice
else
  [ "$#" -eq 1 ] || { echo "$usage" >&2; exit 2; }
  src=$1
  test -d "$src/lib/src/rust" || { echo 'missing lib/src/rust' >&2; exit 2; }
fi

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
dest="$root/lib/upstream"
staging="$root/lib/.upstream.refresh.$$"
backup="$root/lib/.upstream.previous.$$"

command -v python3 >/dev/null 2>&1 || {
  echo 'python3 is required to annotate staged Dart files' >&2
  exit 2
}

# Refuse to destroy attribution we cannot copy. There is deliberately no
# fallback that continues without the license or notice. Reject symlinked
# destinations before reading or copying anything.
if [ -L "$dest" ]; then
  echo "refusing symlinked destination root: $dest" >&2
  exit 3
fi
if [ ! -d "$dest" ]; then
  echo "missing local upstream directory: $dest" >&2
  exit 3
fi
for name in LICENSE NOTICE; do
  if [ -L "$dest/$name" ]; then
    echo "refusing symlinked destination attribution file: $dest/$name" >&2
    exit 3
  fi
  if [ ! -f "$dest/$name" ]; then
    echo "missing local attribution file: $dest/$name" >&2
    exit 3
  fi
done

# Refuse symlinks anywhere in the incoming tree before any preserved metadata
# is copied. Otherwise an upstream LICENSE symlink could redirect that copy
# outside the staging directory.
if [ "$mode" = refresh ]; then
  python3 - "$src/lib/src/rust" <<'PY'
import os
import sys

root = sys.argv[1]


def fail(path):
    sys.stderr.write("refusing symlink in source tree: %s\n" % path)
    raise SystemExit(4)


if os.path.islink(root):
    fail(root)
for dirpath, dirnames, filenames in os.walk(root):
    for name in dirnames:
        path = os.path.join(dirpath, name)
        if os.path.islink(path):
            fail(path)
    for name in filenames:
        path = os.path.join(dirpath, name)
        if os.path.islink(path):
            fail(path)
PY
fi

restore() {
  if [ -d "$backup" ]; then
    if [ ! -d "$dest" ]; then
      if ! mv "$backup" "$dest"; then
        echo "error: failed to restore $dest from $backup; backup preserved for recovery" >&2
        rm -rf "$staging"
        return
      fi
    else
      rm -rf "$backup"
    fi
  fi
  rm -rf "$staging"
}
trap restore EXIT HUP INT TERM

rm -rf "$staging" "$backup"
mkdir -p "$staging"
if [ "$mode" = notice ]; then
  cp -R "$dest/." "$staging/"
else
  cp -R "$src/lib/src/rust/." "$staging/"
  for name in LICENSE NOTICE; do
    if [ -L "$staging/$name" ]; then
      echo "refusing symlinked staging attribution file: $staging/$name" >&2
      exit 4
    fi
    cp "$dest/$name" "$staging/$name"
  done
fi

# Annotate only the staged copy, before the swap. Existing identical prefixes
# are skipped so repeated runs are idempotent.
python3 - "$staging" <<'PY'
import os
import sys

HEADER = (
    b"// Modified for Bloom distribution: this notice was added; local formatting may differ.\n"
    b"// Upstream: OpenBubbles v1.15.0+205 (Apache-2.0); see sidecar ATTRIBUTION and\n"
    b"// lib/upstream/NOTICE.\n"
)

staging = sys.argv[1]
dart_count = 0


def fail(message):
    sys.stderr.write(message + "\n")
    raise SystemExit(4)


for dirpath, dirnames, filenames in os.walk(staging):
    for name in dirnames:
        path = os.path.join(dirpath, name)
        if os.path.islink(path):
            fail("refusing symlinked directory in staged tree: %s" % path)
    for name in filenames:
        if not name.endswith(".dart"):
            continue
        path = os.path.join(dirpath, name)
        if os.path.islink(path):
            fail("refusing symlinked Dart file in staged tree: %s" % path)
        if not os.path.isfile(path):
            fail("Dart entry is not a regular file: %s" % path)
        dart_count += 1
        with open(path, "rb") as handle:
            body = handle.read()
        if body.startswith(HEADER):
            continue
        with open(path, "wb") as handle:
            handle.write(HEADER + body)

if dart_count == 0:
    fail("refusing staged tree with no regular .dart files: %s" % staging)
PY

mv "$dest" "$backup"
mv "$staging" "$dest"
rm -rf "$backup"
trap - EXIT HUP INT TERM
