# Bloom

Bloom is an experimental keyboard-first terminal messenger using an existing OpenBubbles registration. Text messaging, local stored history, automatic engine-child recovery, read-only inbound reaction display, and inbound attachment saving and opening are implemented; attachments use a chooser over loaded files with save or explicit external open, and saving and the chooser are live-verified on inbound files. Remote history import, Bloom-owned sign-in, outbound reactions, and edits/unsend are not; outbound attachment send is implemented with synthetic coverage, but live delivery is unverified.

## Publication and support

Preparation for an experimental source-only publication continues in this
repository. This is not a packaged release or a compatibility guarantee. The
[public-release checklist](docs/public-release-gate.md) records the remaining
reviews and privacy limits. Repository visibility changes require a separate
owner decision.

The sidecar bindings are pinned to OpenBubbles `v1.15.0+205`. Initialization checks
the bridge content hash and rejects a mismatch. Other engine versions are
unverified, and a successful startup does not establish renewal or delivery
reliability. The engine is user-supplied and is not covered by Bloom's MIT license.

See [contributing and support](CONTRIBUTING.md) for safe local development and bug
reports, and [the security policy](SECURITY.md) for private reporting precautions.

## Design reference

The offline, illustrative prototype is at [`design/mockup/index.html`](design/mockup/index.html); preview PNGs are in [`design/previews/`](design/previews/). It is a static reference, not a live service.

**New maintainer or fresh agent:** read [AGENTS.md](AGENTS.md), then the [handoff](docs/handoff.md). The [documentation map](docs/README.md) distinguishes current guides from historical research/plans.

For keyboard controls and troubleshooting, see the [terminal client guide](docs/tui.md). For implemented/partial/missing features and verification boundaries, see [status](docs/status.md). The [system design](docs/system-design.md), [active roadmap](docs/implementation-plan.md), and [switching-latency follow-up](docs/tui-switching-performance.md) explain the architecture, next decisions, and latest accepted work. The [demo mode guide](docs/demo.md) covers the implemented isolated synthetic session behind `--demo`, and the [demo mode plan](docs/demo-mode-plan.md) records its design and isolation acceptance gate.

## Staged direction

1. Make the TUI's text-messaging loop safe, observable, and predictable.
2. Validate engine recovery and historical ingestion before broadening features.
3. Keep independent registration and desktop mockups separate from working-product claims.

See the [reading/navigation review](docs/ux-polish-review.md) and [switching-latency follow-up](docs/tui-switching-performance.md) for the current scope and verification.

## Running it

Prerequisites:

- **Rust**, pinned by `rust-toolchain.toml` (currently 1.97.1). Rustup picks it up
automatically; bump the pin deliberately and re-verify, since CI installs the
same version.
- **Dart 3.13.4**, used by the engine sidecar in `ext/bloom-engine-sidecar/dart`.
The launcher finds `dart` on `PATH` or under `~/.cache/dart-sdk/dart-sdk/bin`.
- **The OpenBubbles Flatpak**, which supplies the engine library and the existing
engine state.

Resolve the sidecar dependencies once with
`(cd ext/bloom-engine-sidecar/dart && dart pub get)`.

From a checkout, `./scripts/bloom` builds and runs the optimized release profile,
starts `bloomd` unless one is already answering on the socket, opens the TUI, and
stops the daemon it started on exit. Pass `--debug` for the debug profile when
backtraces or fast iteration matter; an already running daemon is reused. Daemon diagnostics go to
`~/.local/share/bloom/bloomd.log`. Paths are discovered automatically; put
overrides in a trusted `.env` file at the repository root (`BLOOM_ENGINE_STATE`,
`BLOOM_ENGINE_LIBRARY`, `BLOOM_ENGINE_SCRIPT`, `BLOOM_DART_EXECUTABLE`,
`BLOOM_SOCKET_PATH`). Only paths belong there, never secrets: the file is sourced
as shell.

### Installing the `bloom` command

From the repository root, install the commands and prepare their runtime:

```sh
(cd ext/bloom-engine-sidecar/dart && dart pub get)
export BLOOM_ENGINE_SCRIPT="$PWD/ext/bloom-engine-sidecar/dart/bin/bloom_engine.dart"
cargo install --path apps/bloomd
cargo install --path apps/bloom-tui   # installs both `bloom` and `bloom-tui`
bloom
```

The second command also installs the `bloom` launcher, which starts `bloomd` when
nothing is listening on the socket, opens the TUI, and stops only the daemon it
started. A daemon that is already running is reused and left alone. If another
TUI reuses this daemon, quitting its owning launcher also disconnects that TUI.

**This is not a standalone distribution yet.** Cargo does not install the Dart
sidecar, its packages, Dart itself, or the OpenBubbles engine/registration. Keep
the checkout (including the sidecar's resolved packages), and persist the absolute
`BLOOM_ENGINE_SCRIPT` setting in your shell configuration. The installed launcher
does not source the repository's `.env`; export any needed path overrides. Do not
copy just `bloom_engine.dart`: it imports sibling libraries and generated bindings.
Linux with the OpenBubbles Flatpak is the documented setup; Windows is not supported.

`cargo install` writes binaries to `$CARGO_HOME/bin` (normally `~/.cargo/bin`). Rustup normally puts that
directory on `PATH` already, so `bloom` is runnable by name; if the shell reports
`bloom: command not found`, add it once:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

Put that line in `~/.bashrc`/`~/.zshrc` to make it permanent. With a custom
`CARGO_HOME`, use that directory's `bin` instead. Installing to a custom prefix
(`cargo install --root <dir> ...`) means adding `<dir>/bin` to `PATH` yourself.

**Only one process may own the engine state at a time.** The sidecar loads your
real registration, so stop the OpenBubbles app before starting Bloom, and check
for a leftover `bloomd` or sidecar (`pgrep -f 'bloomd|bloom_engine'`) rather than
assuming the app's absence is enough.

Tests:

```sh
cargo test --workspace --all-targets
cd ext/bloom-engine-sidecar/dart && dart pub get && dart test
```
