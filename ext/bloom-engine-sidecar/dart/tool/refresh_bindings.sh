#!/bin/sh
set -eu
src=${1:?usage: refresh_bindings.sh /path/to/openbubbles-tag-checkout}
test -d "$src/lib/src/rust" || { echo 'missing lib/src/rust' >&2; exit 2; }
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
rm -rf "$root/lib/upstream"
mkdir -p "$root/lib/upstream"
cp -R "$src/lib/src/rust/." "$root/lib/upstream/"
