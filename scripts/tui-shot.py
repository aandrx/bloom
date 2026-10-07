#!/usr/bin/env python3
"""Capture the Bloom TUI as a faithful screen, for style review.

Runs the TUI under a real PTY at a chosen size, feeds keystrokes, replays the
output through a terminal emulator, and writes:

  <out>/screen.txt   the final screen as plain text
  <out>/screen.html  the final screen with real colours, for screenshots

This is review tooling, not product code.
"""

from __future__ import annotations

import argparse
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time
from pathlib import Path

import pyte
from wcwidth import wcswidth

# Terminal-default colors are only a review assumption (notably for NO_COLOR).
# Explicit RGB emitted by the TUI always wins.
DEFAULT_FG = "#e6e6e6"
DEFAULT_BG = "#181818"
KEY_SEQUENCES = {
    "up": b"\x1b[A",
    "down": b"\x1b[B",
    "right": b"\x1b[C",
    "left": b"\x1b[D",
    "pageup": b"\x1b[5~",
    "pagedown": b"\x1b[6~",
    "home": b"\x1b[H",
    "end": b"\x1b[F",
    "enter": b"\r",
    "esc": b"\x1b",
    "tab": b"\t",
    "backspace": b"\x7f",
    "space": b" ",
    "delete": b"\x1b[3~",
    "ctrl-b": b"\x02",
    "ctrl-c": b"\x03",
    "ctrl-w": b"\x17",
    "ctrl-u": b"\x15",
    "ctrl-k": b"\x0b",
    "ctrl-a": b"\x01",
    "ctrl-e": b"\x05",
    "ctrl-n": b"\x0e",
    "ctrl-p": b"\x10",
    "ctrl-j": b"\n",
    "ctrl-left": b"\x1b[1;5D",
    "ctrl-right": b"\x1b[1;5C",
    "alt-left": b"\x1b[1;3D",
    "alt-right": b"\x1b[1;3C",
    "alt-backspace": b"\x1b\x7f",
    "shift-tab": b"\x1b[Z",
    "f2": b"\x1bOQ",
}


def mouse_sequence(token: str) -> bytes | None:
    """SGR mouse input for clicks and wheel events, as `name:x,y` (1-based)."""
    if ":" not in token:
        return None
    name, _, coords = token.partition(":")
    codes = {
        "click": 0,
        "wheelup": 64,
        "wheeldown": 65,
        "rightclick": 2,
    }
    if name not in codes:
        return None
    try:
        x, y = (int(part) for part in coords.split(","))
    except ValueError:
        return None
    press = codes[name]
    # Wheel events are single press reports; buttons also report a release.
    sequence = f"\x1b[<{press};{x};{y}M".encode()
    if name == "click":
        sequence += f"\x1b[<0;{x};{y}m".encode()
    elif name == "rightclick":
        sequence += f"\x1b[<2;{x};{y}m".encode()
    return sequence

PALETTE = {
    "black": "#45475a",
    "red": "#f38ba8",
    "green": "#a6e3a1",
    "brown": "#f9e2af",
    "yellow": "#f9e2af",
    "blue": "#89b4fa",
    "magenta": "#cba6f7",
    "cyan": "#89dceb",
    "white": "#cdd6f4",
}


def normalise_colour(value, fallback: str) -> str:
    if value is None or value == "default":
        return fallback
    if value in PALETTE:
        return PALETTE[value]
    text = str(value).lower()
    if text.startswith("color"):
        index = int(text[5:])
        return xterm_colour(index)
    if len(text) >= 6 and all(c in "0123456789abcdef" for c in text[:6]):
        return f"#{text[:6]}"
    return fallback


def xterm_colour(index: int) -> str:
    basic = [
        "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af",
        "#89b4fa", "#cba6f7", "#89dceb", "#cdd6f4",
    ]
    if index < 8:
        return basic[index]
    if index < 16:
        return basic[index - 8]
    if index < 232:
        index -= 16
        r, g, b = index // 36, (index % 36) // 6, index % 6
        scale = lambda v: 0 if v == 0 else 55 + v * 40  # noqa: E731
        return f"#{scale(r):02x}{scale(g):02x}{scale(b):02x}"
    level = 8 + (index - 232) * 10
    return f"#{level:02x}{level:02x}{level:02x}"


def parse_keys(value: str) -> list[bytes]:
    tokens = value.split()
    if not tokens:
        return []
    # Preserve the historical single-argument literal typing form, including words
    # that happen to match a key name. text: explicitly disambiguates token input.
    if len(tokens) == 1 and not tokens[0].lower().startswith("text:"):
        token = tokens[0]
        if token.lower() not in KEY_SEQUENCES and mouse_sequence(token) is None:
            return [char.encode() for char in token]
    result = []
    for token in tokens:
        mouse = mouse_sequence(token)
        if mouse is not None:
            result.append(mouse)
        elif token.lower().startswith("text:"):
            result.extend(char.encode() for char in token[5:])
        elif token.lower() in KEY_SEQUENCES:
            result.append(KEY_SEQUENCES[token.lower()])
        else:
            result.extend(char.encode() for char in token)
    return result


def run_tui(command: list[str], cols: int, rows: int, keys: list[bytes], settle: float) -> bytes:
    pid, master = pty.fork()
    if pid == 0:
        os.environ["COLUMNS"] = str(cols)
        os.environ["LINES"] = str(rows)
        os.execvp(command[0], command)
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    captured = bytearray()
    deadline = time.time() + settle

    def pump(timeout: float) -> bool:
        ready, _, _ = select.select([master], [], [], timeout)
        if not ready:
            return False
        try:
            chunk = os.read(master, 65536)
        except OSError:
            return False
        if not chunk:
            return False
        captured.extend(chunk)
        return True

    while time.time() < deadline:
        pump(0.1)
    for key in keys:
        os.write(master, key)
        end = time.time() + 0.6
        while time.time() < end:
            pump(0.1)
    end = time.time() + 1.5
    while time.time() < end:
        pump(0.2)

    try:
        os.kill(pid, signal.SIGTERM)
        os.waitpid(pid, 0)
    except OSError:
        pass
    os.close(master)
    return bytes(captured)


def render(raw: bytes, cols: int, rows: int) -> pyte.Screen:
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    stream.feed(raw)
    return screen


def to_html(screen: pyte.Screen, cols: int, rows: int) -> str:
    body = []
    for y in range(rows):
        row = screen.buffer[y]
        spans = []
        continuation = 0
        for x in range(cols):
            if continuation:
                continuation -= 1
                continue
            char = row[x]
            fg = normalise_colour(char.fg, DEFAULT_FG)
            bg = normalise_colour(char.bg, DEFAULT_BG)
            text = char.data or " "
            cells = min(max(1, wcswidth(text)), cols - x)
            continuation = cells - 1
            escaped = text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
            # Terminal cells have fixed geometry, even when browser fallback fonts
            # give bold text or symbols different advances. Never draw an extra
            # space for the continuation cell of a wide character.
            spans.append(
                f'<span style="color:{fg};background:{bg};'
                f'font-weight:{"700" if char.bold else "400"};display:inline-block;'
                f'width:{cells * 9}px;height:18px;vertical-align:top">{escaped}</span>'
            )
        body.append("".join(spans))
    content = "\n".join(body)
    return f"""<!doctype html>
<html><head><meta charset="utf-8"><style>
  body {{ margin:0; background:{DEFAULT_BG}; }}
  pre {{
    margin:0; padding:14px 16px;
    font: 15px/18px "JetBrains Mono","Geist Mono",ui-monospace,SFMono-Regular,Menlo,monospace;
    white-space: pre; color:{DEFAULT_FG}; background:{DEFAULT_BG};
  }}
</style></head><body><pre>{content}</pre></body></html>
"""


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--cols", type=int, default=100)
    parser.add_argument("--rows", type=int, default=30)
    parser.add_argument("--settle", type=float, default=6.0)
    parser.add_argument(
        "--keys", default="", metavar="TOKENS",
        help="whitespace-separated keys or text; key names: up down left right pageup "
             "pagedown home end enter esc tab backspace space delete ctrl-b ctrl-c; "
             "use text:... to type an otherwise ambiguous token",
    )
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()

    command = args.command
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        parser.error("command required after --")

    raw = run_tui(command, args.cols, args.rows, parse_keys(args.keys), args.settle)
    screen = render(raw, args.cols, args.rows)
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "screen.txt").write_text(screen.display and "\n".join(screen.display) + "\n")
    (args.out / "screen.html").write_text(to_html(screen, args.cols, args.rows))
    print((args.out / "screen.txt").read_text())
    return 0


if __name__ == "__main__":
    sys.exit(main())
