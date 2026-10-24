# Standalone packaging plan

Status: design with phase 1 implemented. `bloom --doctor` is accepted; phases 2 to 4 remain unimplemented. Nothing here bundles the engine or claims Cargo alone is sufficient.

## What an install must provide today

- Three Rust binaries: `bloom`, `bloom-tui`, `bloomd` (`README.md:62-68`).
- A Dart runtime and an owned sidecar script that has run `dart pub get` (`README.md:23-35`, `ext/bloom-engine-sidecar/README.md:30-34`).
- An existing OpenBubbles registration and its engine library, plus the state directory (`apps/bloomd/src/config.rs:55-77`).
- Environment or discovery for `BLOOM_ENGINE_STATE`, `BLOOM_ENGINE_LIBRARY`, and `BLOOM_ENGINE_SCRIPT`; `BLOOM_DART_EXECUTABLE` is optional with fallbacks (`config.rs:93-170`).

The launcher finds companion binaries next to itself and then on `PATH` (`apps/bloom-tui/src/launcher.rs:50-84`). The daemon finds the sidecar next to `bin/` as `bloom-engine/bloom_engine.dart`, otherwise at the repository path compiled from `CARGO_MANIFEST_DIR` (`config.rs:79-91`). The engine library is located through `flatpak info --show-location` (`config.rs:64-77`). The installed launcher does not read `.env`; only the repository script does (`README.md:66`, `scripts/bloom:13-19`).

Licensing boundary: the engine is loaded at runtime and must not be bundled or vendored; SSPL and closed components stay process-separated; no MIT crate may depend on `rustpush`. Generated bindings are Apache-2.0 OpenBubbles output, regenerated only by the refresh tool (`ATTRIBUTION:1-3`, `ext/bloom-engine-sidecar/README.md:6-16`).

Existing launcher tests use stub children only. Nothing tests a hidden checkout, a missing Dart runtime, or a real daemon (`scripts/test-launcher-e2e.py:95-226`).

## Design

### Layout

One canonical prefix layout, resolved from the running executable:

```
<prefix>/bin/bloom
<prefix>/bin/bloom-tui
<prefix>/bin/bloomd
<prefix>/share/bloom/engine/bloom_engine.dart   (optional owned payload)
```

Resolution order stays: explicit environment, then `<prefix>` layout, then the legacy sibling `bloom-engine/` directory, then the compiled repository path. Environment always wins, so an operator can point at a sidecar elsewhere.

### Diagnostics first

`bloom --doctor` reports, without starting the daemon or engine:

- companion binaries present and executable;
- sidecar script resolved and readable;
- engine library and state directory resolved;
- Dart executable resolved, with its version when it can be read;
- socket path, store path, and whether a daemon is already listening.

Each check prints one allowlisted line with pass, fail, and the resolution source. Exit status is non-zero when a required check fails. It prints no message bodies, handles, or account data. This is the first unit because it turns today's silent discovery rules into something testable.

### Remaining phases

1. Doctor command: accepted. `bloom --doctor` prints one line per check and exits non-zero only when a required check fails. It starts no daemon or engine, runs only a read-only `flatpak info` lookup, and probes the socket with a connect and no request bytes. Extra arguments after `--doctor` are rejected with exit 2 instead of falling through to a client start. Verified with 126 library, 9 binary, and 120 render tests, strict Clippy, formatting, source guard, build, and a real run on this machine that reported nine passing checks. Review found no defects in the first pass and two low issues in a second pass. Fixed: a missing executable parent no longer skips the repository fallback and produces a false failure. The doctor socket mismatch was fixed next: the daemon, launcher, TUI, and doctor now share `bloom_ipc` resolution, and the doctor prints which source won. A live run with only `BLOOM_SOCKET` set reports that alias path. No packaged install was exercised.
2. Compiled-in layout fallback in daemon configuration, with fake prefixes and a hidden checkout. Design, corrected by review: the pure resolver lives in `bloom_ipc`, not `bloomd`, because `bloom-tui` does not depend on `bloomd` and adding that edge is unnecessary. It takes the resolved `bloomd` executable path, the env value, and the checkout path, and returns the sidecar script in this order: a usable `BLOOM_ENGINE_SCRIPT`, then `<bloomd>/../share/bloom/engine/bloom_engine.dart`, then the legacy `<bloomd>/bloom-engine/bloom_engine.dart`, then the checkout path. The prefix derives from the resolved daemon binary, so launcher and daemon agree even if the launcher lives elsewhere. "Usable" means what the daemon already enforces: trimmed non-empty and valid UTF-8; the doctor adopts that same rule instead of its current `var_os` plus empty-only check. Engine library and state discovery stay Flatpak-based, and the layout adds no bundling. Tests use fake prefixes and a fake executable path, with no real install, sidecar, or engine.
3. Staging script that copies the three binaries plus an optional sidecar payload into a prefix and prints the required environment without writing secrets. Licensing review before distribution claims.
4. Authorized real-engine install check: isolated prefix, unrelated working directory, missing-dependency and version-mismatch cases. This is a separate release gate and needs explicit approval.

### Socket environment alignment (implemented)

Observed mismatch: the TUI client and launcher resolved `BLOOM_SOCKET_PATH`, then `BLOOM_SOCKET`, then the runtime default, while the daemon read only `BLOOM_SOCKET_PATH`. Setting only `BLOOM_SOCKET` pointed the client at a socket the daemon never binds.

Decision: one resolver in `bloom_ipc`, used by the daemon, launcher, and TUI. Precedence is `BLOOM_SOCKET_PATH`, then `BLOOM_SOCKET`, then `default_socket_path()`. Blank or whitespace-only values are treated as unset, which matches how the daemon already behaved. The daemon gaining the alias is the only intended behaviour change. The doctor now uses a source-aware form of the same resolver: `bloom_ipc` returns the path plus which input won (`BLOOM_SOCKET_PATH`, the legacy `BLOOM_SOCKET`, or the runtime default), and the doctor prints that source. Duplicate parsing in the launcher is removed, so a doctor socket line can no longer describe a path the daemon would not bind.\n\n## Explicit unknowns and forbiddens

Unknown: distribution licensing obligations for a packaged sidecar payload, how the Dart runtime is provisioned, and which engine ABI range is supported. The sidecar reports `engine_version` at runtime, but no compatibility matrix exists.

Forbidden: bundling the engine, implying Cargo installs Dart or the sidecar, hand-editing generated bindings, writing credentials to packaged files, starting an engine during packaging checks, and claiming a live install gate from synthetic tests.
