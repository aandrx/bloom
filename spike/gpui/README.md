# Bloom GPUI transcript spike

Disposable, synthetic feasibility spike for a future windowed Bloom client. It is
not a product client and is deliberately outside the root Cargo workspace.

## What it contains

- A 1200x780 GPUI messenger with a conversation list, virtualized transcript,
  optional details pane, and editable composer.
- Responsive pane rules: all three panes at 1100 px or wider, details hidden at
  800–1099 px, and a 160 px sidebar below 800 px. Minimum window width is 480 px.
- 10,000 deterministic synthetic message rows using `uniform_list`.
- Incoming/outgoing surfaces, Unicode and multiline text, edited/unsent states,
  reaction badges, and attachment placeholders.
- A real GPUI `EntityInputHandler` composer with Unicode/grapheme editing,
  selection, mouse placement and marked-text/IME methods. Sending stays disabled.
- Clickable dark/light theme toggle.
- Headless model/slice and windowed frame-callback measurements.

It has no Bloom IPC connection, daemon, engine, account access, sending,
attachment opening, notifications, persistence, accessibility integration, or
production packaging.

## Toolchain and dependencies

The spike is pinned to `gpui = 0.2.2` and builds with Bloom's pinned Rust 1.97.1
on this Linux machine. `libc = 0.2.186` is pinned because GPUI's transitive
`xattr 0.2.3` dependency still references `libc::ENOATTR`, which was removed by
newer libc releases.

GPUI's default Linux features require the normal Wayland/X11 development and
runtime libraries. The release binary links XCB, xkbcommon, xkbcommon-x11, Xau,
Xdmcp, libc, libgcc_s and libm. Ordinary window smoke passed both under the
current X11/Xwayland route and with `DISPLAY=` forcing native Wayland.

## Build and test

From the Bloom repository root:

```sh
cargo check --manifest-path spike/gpui/Cargo.toml --all-targets
cargo test --manifest-path spike/gpui/Cargo.toml
cargo clippy --manifest-path spike/gpui/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path spike/gpui/Cargo.toml -- --check
cargo build --release --manifest-path spike/gpui/Cargo.toml
```

Open the synthetic window:

```sh
spike/gpui/target/release/bloom-gpui-spike
```

Force the native Wayland route:

```sh
DISPLAY= spike/gpui/target/release/bloom-gpui-spike
```

## Measurements

Headless model and six-row slice traversal:

```sh
spike/gpui/target/release/bloom-gpui-spike --measure --rows 10000
```

Observed release result:

```text
rows=10000 build_ms=2.923 slices_ms=0.067 checksum=481e356c24740c83
```

Windowed callback/scroll exercise:

```sh
spike/gpui/target/release/bloom-gpui-spike --frame-measure --rows 10000
```

The frame mode schedules 120 GPUI `on_next_frame` callbacks, scrolls the
`UniformListScrollHandle` through the transcript, and reports the scroll position
the list actually applied as `last_top`. That value is derived from the playback
handle's pixel offset (`-offset_y / row_height`) rather than from the requested
index, so it is a movement check, not an exact first-visible-row index; list
padding and clamping can shift it by a row. A scroll request that never reached
layout would print `last_top=0`. The mode self-fails after 180 seconds, which
only catches a wedged loop: a throttled run has taken 28 s.

The callback interval is **not** GPU render time or compositor presentation
latency. It is strongly affected by window focus, visibility and compositor
throttling.

Observed runs, with identical checksums and identical applied positions:

```text
rows=10000 frames=120 total_ms=2004.566 mean_ms=16.705 p95_ms=17.210 last_top=9986 checksum=a9ca820a646ad6db
rows=10000 frames=120 total_ms=2004.314 mean_ms=16.703 p95_ms=17.057 last_top=9986 checksum=a9ca820a646ad6db
rows=10000 frames=120 total_ms=7310.167 mean_ms=60.918 p95_ms=28.900 last_top=9986 checksum=a9ca820a646ad6db
rows=10000 frames=120 total_ms=28148.577 mean_ms=234.571 p95_ms=1000.795 checksum=a9ca820a646ad6db
```

The 28.1 s run predates the applied-position field. Total runtime varies by more
than an order of magnitude with focus and visibility, so this mode proves the
callback and scroll plumbing works, not that a frame budget is met. A production
decision needs instrumentation tied to presented frames while the window is
focused and visible.

`--rows` accepts 1 through 100000 and is valid only with `--measure` or
`--frame-measure`. The release binary was 25,669,552 bytes. Seventeen unit tests
passed.

## Optional visual capture on niri

Launch the window, find its stable app id, focus it, then use niri's window
screenshot action. Review the output location configured by niri; do not commit
captures containing unrelated desktop content.

```sh
spike/gpui/target/release/bloom-gpui-spike &
PID=$!
ID=$(niri msg -j windows | python3 -c \
  'import json,sys; p=int(sys.argv[1]); xs=json.load(sys.stdin); print(next(x["id"] for x in xs if x.get("pid")==p))' "$PID")
niri msg action focus-window --id "$ID"
niri msg action screenshot-window
kill "$PID"
```

The window title is `Bloom GPUI spike` and app id is
`dev.bloom.gpui-spike`.

## Decision boundary

This spike proves current GPUI can compile, open native Wayland/Xwayland
windows, virtualize a Bloom-shaped transcript, expose responsive pane modes,
and host a real platform-input composer under the pinned toolchain.

It does **not** justify replacing the TUI. Still required before a production
port:

- deterministic visual comparison against the previews;
- presented-frame or GPU timing that is not compositor-throttling-sensitive;
- real manual IME/input-method and text-selection validation;
- accessibility and screen-reader behavior;
- inline image rendering and notification integration;
- shared client-state extraction and daemon IPC reuse;
- production packaging and dependency review.

The TUI remains a supported client regardless of any future GUI decision.
