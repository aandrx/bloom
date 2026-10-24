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
2. Compiled-in layout fallback in daemon configuration, with fake prefixes and a hidden checkout: accepted. The shared resolver lives in `bloom_ipc`, the daemon and doctor both use it, the doctor canonicalises the discovered `bloomd` path so a symlinked install derives the same prefix as the daemon's `/proc/self/exe`, and unusable env values follow the daemon's trimmed UTF-8 rule. Verified with 33 bloom-ipc tests, the bloomd and bloom-tui suites, workspace Clippy, formatting, source guard, diff check, builds, and a doctor run that still resolves the checkout here. Review found no defect after the symlink fix. Design, corrected by review: the pure resolver lives in `bloom_ipc`, not `bloomd`, because `bloom-tui` does not depend on `bloomd` and adding that edge is unnecessary. It takes the resolved `bloomd` executable path, the env value, and the checkout path, and returns the sidecar script in this order: a usable `BLOOM_ENGINE_SCRIPT`, then `<bloomd>/../share/bloom/engine/bloom_engine.dart`, then the legacy `<bloomd>/bloom-engine/bloom_engine.dart`, then the checkout path. The prefix derives from the resolved daemon binary, so launcher and daemon agree even if the launcher lives elsewhere. "Usable" means what the daemon already enforces: trimmed non-empty and valid UTF-8; the doctor adopts that same rule instead of its current `var_os` plus empty-only check. Engine library and state discovery stay Flatpak-based, and the layout adds no bundling. Tests use fake prefixes and a fake executable path, with no real install, sidecar, or engine.
3. Staging script: accepted. `scripts/stage-prefix` and `scripts/test-staging.py` implement the design. Review found and fixed versioned shared-library detection (`.so.1` was slipping through), added refusal tests for a shared library and an engine library inside the prefix, made a file-at-the-prefix path a clean refusal, sanitised attribution values, and stopped the test harness from inheriting `BLOOM_ENGINE_LIBRARY`. 13 tests pass, plus source and diff checks. Staging is not atomic: a mid-copy failure leaves a partial prefix with no manifest, and no rollback claim is made. No real install, engine, or distribution claim was exercised. Original design: `scripts/stage-prefix` takes `--bin-dir`, `--prefix`, optional `--with-sidecar`, and `--dry-run`. It copies `bloom`, `bloom-tui`, and `bloomd` into `<prefix>/bin`, and with `--with-sidecar` copies the owned Dart sidecar sources (`bin/`, `lib/`, `pubspec.*`, `ATTRIBUTION`) into `<prefix>/share/bloom/engine`, never the engine library and never a `.dart_tool` cache, because a build-machine package config is not portable. It refuses a non-empty prefix unless `--force`, and always refuses a prefix inside the source tree, a prefix that already holds state or registration data, and any target where the engine library would land inside the prefix, because that would look like bundling. It writes a manifest of copied paths, SHA-256 digests, and the sidecar `ATTRIBUTION` engine tag or commit, never state, registration, or absolute personal paths. It sets `0755` on binaries and sidecar sources and prints the environment the operator must export. It performs no network access, no `dart pub get`, and starts nothing. The post-install step `dart pub get` stays explicit and documented. A Python test drives the script into temporary prefixes with fake binaries and a fake sidecar tree, covering the layout, the manifest, permission bits, refusal without `--force`, dry-run, and the no-engine rule. Not implemented yet; licensing review comes before any distribution claim.
4. Authorized real-install check: attempted and NOT passed. The gate found a real integration bug: the resolver looked for `share/bloom/engine/bloom_engine.dart` while staging writes `share/bloom/engine/bin/bloom_engine.dart`, because the sidecar's relative imports require the `bin` and `lib` layout. The resolver and launcher test were corrected. With that fix, doctor passed, the staged sidecar was used, `dart pub get --offline` succeeded in the staged tree, and one daemon started, but no engine child appeared, so the TUI never became ready and the run ended in cleanup. No engine log survived because the temporary `XDG_DATA_HOME` was removed. No send, sync, history, registration change, or renewal occurred, and no Bloom process remained afterwards. Next attempt must preserve the daemon log and engine stderr in the temporary root before cleanup. Design: Preconditions: phases 1 to 3 accepted, both binaries and the sidecar staged into a fresh prefix, and explicit user approval for a live start. Before starting, verify one owner with `pgrep -af 'bloomd|bloom_engine'` returning nothing, OpenBubbles closed, and no socket at the chosen path; repeat the process check after exit and abort if any appears mid-run. A live start consumes real pushes, so the temporary Bloom store will contain real messages: delete it with the temporary data directory after the run and never copy, inspect, or share its contents. The prefix and bloom data are isolated through a temporary `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME`, and socket path, but the engine registration itself is the real one and is never relocated, copied, or reset.

Run from an unrelated working directory with `PATH` and the environment pointing only at the staged prefix. Steps: run `bloom --doctor` and require every required check to pass with sources naming the prefix; start the staged `bloom` and confirm it reused the staged sidecar, not the checkout; start it a second time and confirm the listener is reused with one owner; exit and confirm terminal restoration, no surviving `bloomd` or TUI, no stale socket, and that the engine child stopped.

Then break one dependency at a time in a copy of the prefix and require a clean named failure without a partial start: sidecar missing, Dart missing, engine library missing. A version or ABI mismatch cannot be induced safely with the real engine, so it is recorded as unverified unless it occurs.

Evidence: command lines, doctor output, exit codes, process lists before and after, timings, and the reported engine/library version. Record compatibility as unverified rather than passing it. Include no message bodies, handles, account identifiers, credentials, or raw engine logs; engine logs sit outside the TUI allowlist and must be reviewed before any sharing. Forbidden here: sending, syncing, importing history, renewing, registration changes, copying the engine into the prefix, or leaving the daemon running. If any ownership check is ambiguous, abort and report rather than proceed.

### Socket environment alignment (implemented)

Observed mismatch: the TUI client and launcher resolved `BLOOM_SOCKET_PATH`, then `BLOOM_SOCKET`, then the runtime default, while the daemon read only `BLOOM_SOCKET_PATH`. Setting only `BLOOM_SOCKET` pointed the client at a socket the daemon never binds.

Decision: one resolver in `bloom_ipc`, used by the daemon, launcher, and TUI. Precedence is `BLOOM_SOCKET_PATH`, then `BLOOM_SOCKET`, then `default_socket_path()`. Blank or whitespace-only values are treated as unset, which matches how the daemon already behaved. The daemon gaining the alias is the only intended behaviour change. The doctor now uses a source-aware form of the same resolver: `bloom_ipc` returns the path plus which input won (`BLOOM_SOCKET_PATH`, the legacy `BLOOM_SOCKET`, or the runtime default), and the doctor prints that source. Duplicate parsing in the launcher is removed, so a doctor socket line can no longer describe a path the daemon would not bind.\n\n## Explicit unknowns and forbiddens

Unknown: distribution licensing obligations for a packaged sidecar payload, how the Dart runtime is provisioned, and which engine ABI range is supported. The sidecar reports `engine_version` at runtime, but no compatibility matrix exists.

Forbidden: bundling the engine, implying Cargo installs Dart or the sidecar, hand-editing generated bindings, writing credentials to packaged files, starting an engine during packaging checks, and claiming a live install gate from synthetic tests.
