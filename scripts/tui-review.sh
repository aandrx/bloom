#!/bin/sh
# Capture a deterministic matrix of Bloom TUI states and build a review sheet.
set -eu

usage() {
  printf 'Usage: %s OUTPUT_DIR [REFERENCE.png]\n' "${0##*/}" >&2
  printf 'Captures synthetic-fixture TUI states at 100x30 plus responsive picker and Help sizes.\n' >&2
}
fail() { printf 'tui-review: %s\n' "$*" >&2; exit 1; }

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || { usage; exit 2; }
case $1 in -h|--help) usage; exit 0;; esac
out=$1
reference=${2:-}
case $out in ''|/) fail 'output directory must not be empty or /';; esac
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
shot="$script_dir/tui-shot.sh"
[ -x "$shot" ] || fail "not executable: $shot"
command -v python3 >/dev/null 2>&1 || fail 'python3 is required'
command -v node >/dev/null 2>&1 || fail 'node is required by tui-shot.sh'

# Canonicalize the requested directory before passing it to the capture script.
mkdir -p -- "$out" || fail "cannot create output directory: $out"
out=$(CDPATH= cd -- "$out" && pwd -P) || fail "cannot resolve output directory: $out"
ref_note='Reference image omitted: none supplied.'
if [ -n "$reference" ]; then
  if [ -f "$reference" ] && [ -r "$reference" ]; then
    case $(python3 -c 'import sys; print(open(sys.argv[1],"rb").read(8).hex())' "$reference" 2>/dev/null || true) in
      89504e470d0a1a0a) reference=$(CDPATH= cd -- "$(dirname -- "$reference")" && printf '%s/%s\n' "$(pwd -P)" "$(basename -- "$reference")"); ref_note='';;
      *) reference=''; ref_note='Reference image omitted: supplied file is not a readable PNG.';;
    esac
  else
    reference=''; ref_note='Reference image omitted: supplied file is missing or unreadable.'
  fi
fi

# Never read or update the reviewer's personal preferences or diagnostic files.
export XDG_CONFIG_HOME="$out/.config" XDG_STATE_HOME="$out/.state" XDG_DATA_HOME="$out/.data"
mkdir -p "$XDG_CONFIG_HOME/bloom" "$XDG_STATE_HOME" "$XDG_DATA_HOME"
printf '%s\n' '{"version":1,"theme":"dark"}' >"$XDG_CONFIG_HOME/bloom/tui.json"

capture() {
  name=$1 cols=$2 rows=$3 keys=$4
  dest="$out/$name"
  mkdir -p -- "$dest" || fail "cannot create state directory: $dest"
  "$shot" "$dest" "$cols" "$rows" "$keys" >/dev/null || fail "capture failed: $name"
  [ -s "$dest/tui.png" ] || fail "capture produced no PNG: $name"
}

capture default 100 30 ''
capture settings-appearance 100 30 'text:,'
capture settings-layout 100 30 'text:, right'
capture contacts 100 30 'text:/contacts enter'
capture filter 100 30 'text:/filter enter'
capture filter-no-match 100 30 'text:/filter enter text:unmatched'
capture slash-menu 100 30 'text:/'
capture theme 100 30 'text:/theme enter'
capture help 100 30 'text:?'
capture settings-appearance-66x18 66 18 'text:,'
capture settings-layout-66x18 66 18 'text:, right'
capture contacts-66x18 66 18 'text:/contacts enter'
capture help-80x4 80 4 'text:?'
capture filter-66x18 66 18 'text:/filter enter'
capture filter-44x12 44 12 'text:/filter enter'
# Dedicated narrow regression captures. The long-title fixture is opt-in so the
# canonical fake-daemon review states above remain unchanged.
BLOOM_FAKE_LONG_TITLE=1 capture filter-44x12-long-title 44 12 'text:/filter enter down'
contacts_config="$out/.contacts-config"
mkdir -p -- "$contacts_config/bloom"
printf '%s\n' '{"version":1,"nicknames":{"email:person0@example.invalid":"Maya Chen"}}' >"$contacts_config/bloom/tui.json"
XDG_CONFIG_HOME="$contacts_config" capture contacts-44x12-nickname 44 12 'text:/contacts enter'
XDG_CONFIG_HOME="$contacts_config" capture contacts-search 100 30 'text:/contacts enter text:person0'
XDG_CONFIG_HOME="$contacts_config" capture contacts-search-no-match 100 30 'text:/contacts enter text:no-such-contact'
python3 - "$out/filter-44x12-long-title/screen.txt" "$out/contacts-44x12-nickname/screen.txt" "$out/contacts-search/screen.txt" "$out/contacts-search-no-match/screen.txt" <<'PY'
import re
import sys
from pathlib import Path

filter_lines = Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
filter_row = next((line for line in filter_lines if "› Direct chat with a" in line), None)
assert filter_row and filter_row.rstrip().endswith("Direct"), (
    "Filter truncation must preserve the Direct type suffix: "
    f"{filter_row!r}"
)

contact_lines = Path(sys.argv[2]).read_text(encoding="utf-8").splitlines()
contact_row = next((line for line in contact_lines if "Maya Chen" in line), None)
assert contact_row and re.search(r"Maya Chen\s+mailto:person0@", contact_row), (
    "Contacts nickname and clipped address must share a non-overlapping row: "
    f"{contact_row!r}"
)

search_lines = Path(sys.argv[3]).read_text(encoding="utf-8").splitlines()
search_row = next((line for line in search_lines if "mailto:person0@" in line), None)
assert any("Search: person0" in line for line in search_lines), (
    "Contacts must show the active number query"
)
assert search_row and re.search(r"Maya Chen\s+mailto:person0@", search_row), (
    "Searching a number must retain the matching nickname and address on one row: "
    f"{search_row!r}"
)

empty_lines = Path(sys.argv[4]).read_text(encoding="utf-8").splitlines()
assert any("No contacts match" in line for line in empty_lines), (
    "An unmatched Contacts query must show an empty state"
)
PY
capture theme-66x18 66 18 'text:/theme enter'
capture theme-44x12 44 12 'text:/theme enter'
capture default-66x18 66 18 ''
capture default-44x12 44 12 ''
capture new-recipient 100 30 'text:/new enter'
capture new-body 66 18 'text:/new enter text:carol@example.invalid enter text:Hello space text:Carol'
capture multiline 66 18 'text:First space text:line ctrl-j text:Second space text:line'
BLOOM_FAKE_SESSION_STATE=failed capture session-failed-44x12 44 12 ''
BLOOM_FAKE_LONG_TITLE=1 capture long-title-44x12 44 12 'tab down enter'
NO_COLOR=1 capture no-color-picker 66 18 'text:/theme enter'
light_config="$out/.light-config"
mkdir -p "$light_config/bloom"
printf '%s\n' '{"version":1,"theme":"light"}' >"$light_config/bloom/tui.json"
XDG_CONFIG_HOME="$light_config" capture light 100 30 ''
XDG_CONFIG_HOME="$light_config" capture light-settings 66 18 'text:, right'

export REVIEW_OUT="$out" REVIEW_REFERENCE="$reference" REVIEW_NOTE="$ref_note"
python3 - <<'PY'
import html, os
from pathlib import Path
out=Path(os.environ['REVIEW_OUT'])
states=['default','settings-appearance','settings-layout','contacts','filter','filter-no-match','slash-menu','theme','help','settings-appearance-66x18','settings-layout-66x18','contacts-66x18','help-80x4','filter-66x18','filter-44x12','filter-44x12-long-title','contacts-44x12-nickname','contacts-search','contacts-search-no-match','theme-66x18','theme-44x12','default-66x18','default-44x12','new-recipient','new-body','multiline','session-failed-44x12','long-title-44x12','no-color-picker','light','light-settings']
ref=os.environ['REVIEW_REFERENCE']
note=os.environ['REVIEW_NOTE']
def card(label, path):
    return f'<section><h2>{html.escape(label)}</h2><img src="{html.escape(path)}" alt="{html.escape(label)}"></section>'
parts=[]
if ref:
    parts.append(card('Codex reference', Path(ref).as_uri()))
parts.extend(card(name.replace('-', ' ').title(), f'{name}/tui.png') for name in states)
message=f'<p class="note">{html.escape(note)}</p>' if note else ''
page='''<!doctype html><meta charset="utf-8"><title>Bloom TUI review</title><style>body{font:14px system-ui;background:#222;color:#eee;margin:20px}main{display:grid;grid-template-columns:repeat(auto-fit,minmax(470px,1fr));gap:18px}section{background:#333;padding:10px;border-radius:6px}h2{font-size:16px;margin:0 0 8px;text-transform:capitalize}img{display:block;width:100%;height:auto;background:#1e1e2e}.note{color:#ffd28a}</style>'''+message+'<main>'+''.join(parts)+'</main>'
(out/'contact-sheet.html').write_text(page,encoding='utf-8')
PY
# Render the same sheet to PNG through the existing browser tooling.
browser=$HOME/.pi/agent/skills/browser-tools/browser.js
[ -f "$browser" ] || fail "browser tool not found: $browser"
node "$browser" start --headless >/dev/null 2>&1 || fail 'could not start browser'
trap 'node "$browser" stop >/dev/null 2>&1 || true' EXIT INT TERM
node "$browser" nav "file://$out/contact-sheet.html" >/dev/null 2>&1 || fail 'could not open contact sheet'
node "$browser" viewport 1600 3000 >/dev/null 2>&1 || fail 'could not set contact-sheet viewport'
node "$browser" screenshot --path "$out/contact-sheet.png" >/dev/null 2>&1 || fail 'could not render contact sheet PNG'
node "$browser" stop >/dev/null 2>&1 || true
trap - EXIT INT TERM
[ -s "$out/contact-sheet.png" ] || fail 'contact-sheet PNG is empty'
printf 'Review outputs: %s\n' "$out"
printf 'Contact sheet: %s/contact-sheet.png\nHTML: %s/contact-sheet.html\n' "$out" "$out"
