#!/bin/sh
# Capture the Bloom TUI as a screenshot for style review.
#
# Runs a fake daemon with synthetic fixtures, drives the real TUI under a PTY,
# replays the output through a terminal emulator, and renders a PNG. The fixtures
# contain no real messages or handles.
#
# Usage: tui-shot.sh [output-dir] [cols] [rows] [keys]
# Keys are whitespace-separated tokens: up down left right pageup pagedown home
# end enter esc tab backspace space delete ctrl-b ctrl-c. Use text:... to type a
# literal ambiguous token; a single non-key token remains literal text.
# Prints the path of the produced PNG.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

out=${1:-$HOME/.cache/bloom-shot}
cols=${2:-100}
rows=${3:-30}
keys=${4:-}

tools=$HOME/.cache/bloom-tools/bin/python
if [ ! -x "$tools" ]; then
  python3 -m venv $HOME/.cache/bloom-tools
  $HOME/.cache/bloom-tools/bin/pip install -q pyte
fi

CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2} cargo build -p bloom-tui

mkdir -p "$out"
chmod 700 "$out"
socket="$out/fake.sock"
rm -f "$socket"

./scripts/fake-daemon.py "$socket" >"$out/daemon.log" 2>&1 &
fake=$!
trap 'kill "$fake" 2>/dev/null || true' EXIT INT TERM
sleep 1

BLOOM_SOCKET_PATH="$socket" "$tools" scripts/tui-shot.py \
  --out "$out" --cols "$cols" --rows "$rows" --settle 4 --keys "$keys" \
  -- ./target/debug/bloom-tui >"$out/screen.txt"

# Size the viewport to the content so the screenshot has no dead margin.
width=$((cols * 9 + 34))
height=$((rows * 18 + 36))
browser=$HOME/.pi/agent/skills/browser-tools/browser.js
node "$browser" start --headless >/dev/null 2>&1 || true
node "$browser" nav "file://$out/screen.html" >/dev/null 2>&1
node "$browser" viewport "$width" "$height" >/dev/null 2>&1
node "$browser" screenshot --path "$out/tui.png" >/dev/null 2>&1
node "$browser" stop >/dev/null 2>&1 || true

echo "$out/tui.png"
