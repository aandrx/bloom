# Bloom

Bloom is a keyboard-first messaging concept pairing a calm desktop conversation view with a terminal-oriented transcript.

## Design reference

The offline, illustrative prototype is at [`design/mockup/index.html`](design/mockup/index.html); preview PNGs are in [`design/previews/`](design/previews/). It is a static reference, not a live service.

For keyboard controls, screen details, and troubleshooting, see the [terminal client guide](docs/tui.md). For what is built, what is verified and what is blocked, see the [status page](docs/status.md).

## Staged direction

1. Build an auth-first foundation, establishing secure identity and session flows before messaging functionality.
2. Develop the TUI-first experience around keyboard-led navigation and conversation workflows.

## Running it

Prerequisites:

- **Rust**, pinned by `rust-toolchain.toml` (currently 1.97.1). Rustup picks it up
automatically; bump the pin deliberately and re-verify, since CI installs the
same version.
- **Dart 3.13.4**, used by the engine sidecar in `ext/bloom-engine-sidecar/dart`.
The launcher finds `dart` on `PATH` or under `~/.cache/dart-sdk/dart-sdk/bin`.
- **The OpenBubbles Flatpak**, which supplies the engine library and the existing
engine state.

```sh
./scripts/bloom
```

That rebuilds both binaries if their sources changed, starts `bloomd` unless one
is already answering on the socket, opens the TUI, and stops the daemon it
started on exit. Daemon diagnostics go to `~/.local/share/bloom/bloomd.log`.
Paths are discovered automatically; put overrides in a `.env` file next to the
script (`BLOOM_ENGINE_STATE`, `BLOOM_ENGINE_LIBRARY`, `BLOOM_ENGINE_SCRIPT`,
`BLOOM_DART_EXECUTABLE`, `BLOOM_SOCKET_PATH`). Only paths belong there, never
secrets: the file is sourced as shell.

**Only one process may own the engine state at a time.** The sidecar loads your
real registration, so stop the OpenBubbles app before starting Bloom, and check
for a leftover `bloomd` or sidecar (`pgrep -f 'bloomd|bloom_engine'`) rather than
assuming the app's absence is enough.

Tests:

```sh
cargo test --workspace --all-targets
cd ext/bloom-engine-sidecar/dart && dart pub get && dart test
```
