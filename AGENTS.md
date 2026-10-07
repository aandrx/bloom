# Bloom agent handoff

## Read first

1. `docs/handoff.md`: intent, current capabilities, known gaps, safe setup and next work.
2. `docs/status.md`: evidence boundaries. Synthetic tests are not live-account proof.
3. `docs/system-design.md`: current implementation and process/licensing boundary.
4. `docs/implementation-plan.md`: ordered roadmap and acceptance gates.
5. `docs/tui.md` and `docs/tui-internals.md` when changing the terminal client.

`docs/README.md` indexes all documents and identifies historical records. Do not execute old wave/task lists or treat static design previews as implemented clients.

## Safety rules

- Do not run `bloom`, `bloomd`, the real sidecar, ignored real-engine tests, registration, renewal, or live send/receive without explicit user authorization. Engine startup reads/writes registration and consumes incoming pushes.
- Use synthetic fixtures, temporary owned socket directories and isolated XDG paths for tests. Never put real message bodies, real recipient IDs, credentials, cookies, keys or raw engine frames in diagnostics or Git. Clearly synthetic fixture messages/identities are permitted in tests.
- No destructive database/state reset, new registration, automatic uncertain-send retry, or guessed recipient merge. Existing encrypted database with missing key must fail closed.
- Preserve external engine ownership: only one process may open a registration. Different sockets do not establish different accounts or safe concurrent ownership.
- Engine library stays external. Never vendor it, introduce `rustpush` into the default daemon graph, or modify generated files under `ext/bloom-engine-sidecar/dart/lib/upstream/`. Follow `ATTRIBUTION` and the refresh tool for version changes.
- Inspect Git before editing. Preserve existing user changes. Give each delegated writer disjoint files, serialize verification, and inspect actual diffs/results before accepting work. Avoid launching visual jobs while changing their scripts or renderer.
- Never edit auto-generated changelogs. Do not add an agent co-author to commits. Use plain dash punctuation.

## Verification

Rust toolchain is pinned in `rust-toolchain.toml`; Dart version is pinned in CI.

```sh
cargo build -p bloom-tui
cargo test --workspace --all-targets
TZ=Asia/Tokyo cargo test -p bloom-tui
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
./scripts/check-sources
```

Use a Python virtual environment with `pyte==0.8.2` for all Python suites:

```sh
python scripts/test-stateful-fake-daemon.py
python scripts/test-tui-input-e2e.py
python scripts/test-tui-messaging-e2e.py
python scripts/test-launcher-e2e.py
```

Dart owned-source checks and visual capture instructions are in `docs/handoff.md`.
The mutation guard script refuses dirty files; never reset working changes to run it.

## Change discipline

Reproduce user-facing defects in an isolated E2E flow first, then add a regression and fix. Review small/normal/light/dark/monochrome UI states and actual mouse hitboxes. Keep acceptance, persistence and delivery distinct. For sends, preserve attempt correlation, recipient ownership, literal-paste provenance, newer drafts and explicit uncertainty. Update current docs and the handoff when behavior or priorities change; historical evidence remains labeled as historical.
