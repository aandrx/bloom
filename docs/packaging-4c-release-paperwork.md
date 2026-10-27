# B-4c release paperwork: licensing, engine range and the Cargo boundary

Status: review complete. **Distribution is not ready.** No distribution claim is
made, no engine library is bundled, and no live install was rerun for this unit.
The review found missing license and notice artifacts and unresolved obligations
around the external OpenBubbles engine.

## Question

What must accompany a staged Bloom prefix for it to be distributable, which
OpenBubbles engine versions are supported, and what does Cargo alone fail to
provide?

## Observed facts

### Bloom's own license

- The workspace declares MIT in `Cargo.toml:16`.
- `bloom-core` and `bloom-tui` were missing the workspace license field; that
  metadata is fixed and `cargo metadata --no-deps` now reports MIT for all seven
  workspace packages.
- There is no root `LICENSE` file anywhere in the repository. The MIT declaration
  has no accompanying license text or copyright notice, which is a distribution
  blocker.

### Vendored upstream bindings

- `ext/bloom-engine-sidecar/ATTRIBUTION` records generated Dart bindings from
  OpenBubbles v1.15.0+205, commit `852c9910f456492f7f840ac4ff2a8bad7a1ee8c8`,
  under Apache-2.0.
- The repository contains no Apache-2.0 license text and no upstream NOTICE file.
- The staging dry run copies `lib/upstream`, `pubspec.*` and `ATTRIBUTION` into
  `share/bloom/engine`, but no license or notice file.

### Staged contents

`python3 scripts/stage-prefix --bin-dir target/debug --prefix <tmp> --with-sidecar --dry-run` plans:

- `bin/bloom`, `bin/bloom-tui`, `bin/bloomd`;
- `share/bloom/engine/bin/bloom_engine.dart`, the owned Dart libraries, the
  generated upstream bindings, `pubspec.yaml`, `pubspec.lock` and `ATTRIBUTION`;
- `stage-manifest.json` with copied paths, SHA-256 digests and the upstream tag or
  commit.

It plans no license files and no engine library. `scripts/test-staging.py` passes
13 synthetic tests, including refusal to stage a shared library or an engine
library inside the prefix.

### The external engine

- The sidecar README states that the engine library is loaded at runtime only and
  is never included, copied, downloaded or bundled.
- The same README states that the engine contains SSPL and closed-source
  components. Bloom does not link against it; it stays behind the child process
  boundary.
- The installed Flatpak reports version `1.15.0.0`; the sidecar bindings were
  generated from source tag `v1.15.0+205`. The relationship between the Flatpak
  version string and the upstream source tag is not independently verified here.

### Supported engine range

- Bindings are pinned to the OpenBubbles tag above.
- `ext/bloom-engine-sidecar/dart/lib/upstream/frb_generated.dart:69` carries a
  Rust content hash. Flutter Rust Bridge checks it at `RustLib.init`
  (`flutter_rust_bridge-2.3.0/lib/src/main_components/entrypoint.dart:103-112`); a mismatch throws before commands are served.
- The sidecar does not catch that initialization failure, so a mismatched engine
  exits with a stack trace rather than a bounded ready-frame error.
- `bloom_engine.dart:29` reports a hardcoded `engineVersion` constant in the
  `ready` frame. It is not read from the installed library.
- The doctor checks that the library path exists, but it does not verify the
  content hash or a supported version. The first real check happens at sidecar
  initialization.
- Supported range: exactly the pinned binding tag. Other engine tags are
  unverified, even if they load; a different FRB content hash should fail closed,
  but that failure is not surfaced cleanly and no compatibility matrix exists.

### Rust and Dart dependencies

- `cargo metadata` reports 492 packages. The dominant license expressions are
  `MIT OR Apache-2.0` and `MIT`; the inventory also contains `MPL-2.0` (5),
  `Unicode-3.0` (18), `Zlib`, `BSD-3-Clause`, `CDLA-Permissive-2.0` and other
  expressions.
- No generated third-party license or notice bundle exists for the Rust graph.
  MPL-2.0 and Unicode-3.0 obligations, at minimum, need review before binary
  distribution.
- The Dart packages (`flutter_rust_bridge`, `freezed_annotation`, `lints`, `test`)
  are not vendored or staged. The operator runs `dart pub get`; package licenses
  arrive with that resolved tree, not with the staged prefix.

### Cargo alone

Cargo installs or builds Bloom's Rust binaries only. It does not install:

- Dart 3.13.4;
- the sidecar source tree or its resolved pub packages;
- the OpenBubbles Flatpak, its engine library, or an existing registration;
- the exclusive engine-state ownership needed at runtime.

The documented runtime also requires the pinned engine state and library plus a
`dart pub get` in the staged sidecar tree.

## Blockers before any distribution

1. **Bloom license text.** Choose the copyright holder and add an MIT `LICENSE`
   file; propagate it to the staged prefix.
2. **Upstream license and notice.** Add the Apache-2.0 license text and any
   upstream NOTICE required for the vendored bindings, and stage them with
   `lib/upstream`.
3. **Third-party notices.** Generate and review a Rust/Dart third-party license
   bundle. At minimum, resolve the MPL-2.0 and Unicode-3.0 obligations and the
   packages with compound expressions.
4. **Engine obligations.** Obtain a legal review of distributing Bloom binaries
   and sidecar sources that interact with an SSPL/closed engine. Do not bundle the
   engine. The process boundary is an engineering control, not a legal opinion.
5. **Engine preflight.** Add a bounded version or content-hash check before
   serving commands, with a clear unsupported-engine failure. Treat the hardcoded
   `engineVersion` as a label, not proof of what was loaded.
6. **Packaging gate.** Re-run the 4b staged-prefix and lifecycle checks after any
   packaging change. The phase 4 fault remains intermittent and the containment
   path was not exercised live.

## Decision

B-4c is a review, not a release. Distribution remains blocked by missing license
and notice artifacts, unresolved third-party obligations, the unresolved
SSPL/closed-engine review, and an unverified compatibility range. The safe
statement is: Bloom source uses an MIT license declaration; the vendored bindings
are Apache-2.0 from the pinned OpenBubbles tag; the engine is external,
user-supplied, and not covered by Bloom's license.

No distribution claim, no Cargo-only installation claim, and no supported-version
range beyond the pinned tag should be made.

## Verification

Observed with `cargo metadata`, a staging dry run, `scripts/test-staging.py`
(13 passing tests), the sidecar `ATTRIBUTION` and README, Flutter Rust Bridge
the Flutter Rust Bridge `entrypoint.dart`, the generated binding content hash, the sidecar's engine-version
constant, and the launcher doctor checks. No legal advice is given. No real install,
engine start, account access or distribution was performed for this record.
