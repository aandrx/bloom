# Bloom GPUI transcript spike

Disposable, synthetic feasibility spike for a future windowed Bloom client. It is
not a product client and is deliberately outside the root Cargo workspace.

## What it contains

- A 1200x780 dark GPUI window with a 220 px conversation list, virtualized
  transcript, 260 px details panel, and static composer.
- 10,000 deterministic synthetic message rows using `uniform_list`.
- Incoming/outgoing surfaces, Unicode and multiline text, edited/unsent states,
  reaction badges, and attachment placeholders.
- A headless model/viewport benchmark.

It has no Bloom IPC connection, daemon, engine, account access, sending,
attachment opening, notifications, persistence, editable composer, theme toggle,
or production packaging.

## Toolchain and dependencies

The spike is pinned to `gpui = 0.2.2` and builds with Bloom's pinned Rust 1.97.1
on this Linux machine. `libc = 0.2.186` is pinned because GPUI's transitive
`xattr 0.2.3` dependency still references `libc::ENOATTR`, which was removed by
newer libc releases.

GPUI's default Linux features require the normal Wayland/X11 development and
runtime libraries. The release binary currently links XCB, xkbcommon,
xkbcommon-x11, Xau, Xdmcp, libc, libgcc_s and libm. The observed window ran
through X11/Xwayland in a Wayland session.

## Commands

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

Run the headless measurement:

```sh
spike/gpui/target/release/bloom-gpui-spike --measure --rows 10000
```

`--rows` accepts 1 through 100000 and is valid only with `--measure`.
The output contains only row count, model construction time, 100 viewport-slice
time, and a deterministic checksum.

## Observed result

On this machine, one release measurement for 10,000 rows reported:

```text
rows=10000 build_ms=1.937 slices_ms=0.046 checksum=481e356c24740c83
```

The release binary was 25,343,504 bytes. Eight unit tests passed. The window
process stayed alive for an 8-second X11/Xwayland smoke run without stderr.

The benchmark uses six-row slices, approximately the visible transcript at this row height. These figures measure synthetic model construction and slice traversal, not GPUI
frame rendering, compositor paint, scrolling latency, memory, input methods,
accessibility, Wayland-native behavior, or real Bloom state. Repeat measurements
before comparing toolkits.

## Decision boundary

This spike proves that current GPUI can compile and open a virtualized synthetic
Bloom-shaped surface under the pinned toolchain. It does not justify replacing
the TUI. A production decision still needs measured GPUI frame behavior, real
text selection and input methods, accessibility, screenshot review against the
previews, daemon-client integration, and packaging impact. The TUI remains a
supported client regardless of a future GUI decision.
